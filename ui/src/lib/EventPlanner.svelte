<script lang="ts">
  /**
   * EventPlanner — scheduling-assistant modal for issue #137.
   *
   * Spawned from EventEditor's "Find time" button.  Renders each
   * invitee's busy / free across a focused day so the user can pick
   * a time slot that works for everyone, then hands the chosen
   * start/end back to the editor via `onapply`.
   *
   * Layout:
   *   - Top: 7-day scrub strip showing aggregate busy density
   *     per day; click a day to focus the grid on it.
   *   - Middle: attendees as columns, 24h time as vertical axis,
   *     busy periods overlaid as coloured blocks per attendee.
   *   - Floating proposed-slot band stretching across all
   *     attendee columns at the picked time.  Click anywhere on
   *     the grid to relocate the slot (duration preserved).
   *
   * Data flow:
   *   - On open, calls `get_attendee_availability` (a Tauri IPC
   *     command) for the full 7-day window so day-switching
   *     within the scrub strip is instant.
   *   - The backend resolves each attendee to either a CalDAV
   *     free-busy-query (NC users) or a local-cache scan
   *     (externals), so what we receive is already aggregated.
   */
  import { invoke } from '@tauri-apps/api/core'
  import { m } from '../paraglide/messages'

  interface EventAttendee {
    email: string
    common_name?: string | null
    role?: string | null
    status?: string | null
  }

  interface AttendeeBusyPeriod {
    start: string
    end: string
    kind: 'busy' | 'tentative' | 'unavailable' | 'free'
  }

  interface AttendeeAvailability {
    email: string
    displayName: string | null
    source: 'nc-freebusy' | 'local-cache' | 'unknown'
    busyPeriods: AttendeeBusyPeriod[]
  }

  interface Props {
    open: boolean
    ncId: string
    attendees: EventAttendee[]
    proposedStart: Date
    proposedEnd: Date
    onclose: () => void
    onapply: (start: Date, end: Date) => void
  }

  let {
    open,
    ncId,
    attendees,
    proposedStart,
    proposedEnd,
    onclose,
    onapply,
  }: Props = $props()

  // ── State ────────────────────────────────────────────────────
  // The picked time / focus day clone the prop values; we
  // initialise them in an `$effect.pre` so Svelte's reactivity
  // tracker doesn't read the props directly in a `$state(...)`
  // expression (which warns about "captures only the initial
  // value").  In practice the EventEditor mounts/unmounts the
  // planner with `{#if plannerOpen}`, so this effect fires once
  // on each open with the editor's current proposed slot.
  let focusDay = $state<Date>(new Date(0))
  let pickedStart = $state<Date>(new Date(0))
  let pickedEnd = $state<Date>(new Date(0))
  let eventDurationMs = $state(15 * 60 * 1000)
  let availability = $state<AttendeeAvailability[]>([])
  let loading = $state(false)
  let loadError = $state<string | null>(null)

  $effect.pre(() => {
    focusDay = stripTime(proposedStart)
    pickedStart = new Date(proposedStart)
    pickedEnd = new Date(proposedEnd)
    eventDurationMs = Math.max(
      // Floor: 15 min so the band is always visible.
      15 * 60 * 1000,
      proposedEnd.getTime() - proposedStart.getTime(),
    )
  })

  // ── Layout constants ─────────────────────────────────────────
  // 32px per hour gives 24 × 32 = 768 px of total grid height —
  // tall enough that a 1-hour block is comfortably clickable
  // without dwarfing the rest of the modal chrome.
  const HOUR_PX = 32
  const GRID_PX = HOUR_PX * 24
  const ATTENDEE_COL_PX = 160
  const TIME_GUTTER_PX = 56

  // ── Week range ───────────────────────────────────────────────
  // We anchor the 7-day scrub strip on `focusDay` minus 3 days so
  // the focused day always sits in the middle slot.  Re-centres
  // when the user nudges the focus across the strip's edge.
  let weekStart = $derived.by(() => {
    const d = stripTime(focusDay)
    d.setDate(d.getDate() - 3)
    return d
  })
  let weekEnd = $derived.by(() => {
    const d = new Date(weekStart)
    d.setDate(d.getDate() + 7)
    return d
  })
  let weekDays = $derived.by(() => {
    const days: Date[] = []
    for (let i = 0; i < 7; i++) {
      const d = new Date(weekStart)
      d.setDate(d.getDate() + i)
      days.push(d)
    }
    return days
  })

  // ── Data loading ─────────────────────────────────────────────
  async function loadAvailability(start: Date, end: Date) {
    if (attendees.length === 0) {
      availability = []
      return
    }
    loading = true
    loadError = null
    try {
      const result = await invoke<AttendeeAvailability[]>('get_attendee_availability', {
        ncId,
        attendeeEmails: attendees.map((a) => a.email),
        rangeStart: start.toISOString(),
        rangeEnd: end.toISOString(),
      })
      availability = result
    } catch (e) {
      loadError = String(e)
      availability = []
    } finally {
      loading = false
    }
  }

  // Reload when the modal opens or the visible week shifts.
  $effect(() => {
    if (open) {
      void loadAvailability(weekStart, weekEnd)
    }
  })

  // ── Event handlers ───────────────────────────────────────────
  function focusDifferentDay(day: Date) {
    focusDay = stripTime(day)
    // Keep the picked slot's clock-time but move it to the new day
    // so the user can compare days without re-positioning.
    const hh = pickedStart.getHours()
    const mm = pickedStart.getMinutes()
    const next = new Date(day)
    next.setHours(hh, mm, 0, 0)
    pickedStart = next
    pickedEnd = new Date(next.getTime() + eventDurationMs)
  }

  function gridClick(e: MouseEvent) {
    const grid = e.currentTarget as HTMLElement
    const rect = grid.getBoundingClientRect()
    const y = e.clientY - rect.top
    // Snap to 15-minute increments for a forgiving click target.
    const minutesFromMidnight = Math.max(
      0,
      Math.min(24 * 60 - 15, Math.round((y / GRID_PX) * 24 * 60 / 15) * 15),
    )
    const next = new Date(focusDay)
    next.setHours(0, minutesFromMidnight, 0, 0)
    pickedStart = next
    pickedEnd = new Date(next.getTime() + eventDurationMs)
  }

  function close() {
    onclose()
  }
  function apply() {
    onapply(pickedStart, pickedEnd)
    onclose()
  }

  // ── Helpers ──────────────────────────────────────────────────
  function stripTime(d: Date): Date {
    const out = new Date(d)
    out.setHours(0, 0, 0, 0)
    return out
  }

  function sameDay(a: Date, b: Date): boolean {
    return (
      a.getFullYear() === b.getFullYear()
      && a.getMonth() === b.getMonth()
      && a.getDate() === b.getDate()
    )
  }

  function busyForDay(periods: AttendeeBusyPeriod[], day: Date): AttendeeBusyPeriod[] {
    const dayStart = stripTime(day).getTime()
    const dayEnd = dayStart + 24 * 60 * 60 * 1000
    return periods.filter((p) => {
      const ps = new Date(p.start).getTime()
      const pe = new Date(p.end).getTime()
      return pe > dayStart && ps < dayEnd
    })
  }

  function busyMinutesForDay(av: AttendeeAvailability, day: Date): number {
    const dayStart = stripTime(day).getTime()
    const dayEnd = dayStart + 24 * 60 * 60 * 1000
    let total = 0
    for (const p of av.busyPeriods) {
      const ps = Math.max(new Date(p.start).getTime(), dayStart)
      const pe = Math.min(new Date(p.end).getTime(), dayEnd)
      if (pe > ps) total += (pe - ps) / 60000
    }
    return total
  }

  /** Aggregate busy share across all attendees for a given day,
   *  expressed as a fraction in [0, 1].  Used to colour the
   *  scrub strip's per-day cells: more crowded → darker. */
  function aggregateBusy(day: Date): number {
    if (availability.length === 0) return 0
    const totalMinutesPerAttendee = 24 * 60
    const totalBusy = availability.reduce(
      (acc, av) => acc + busyMinutesForDay(av, day),
      0,
    )
    return totalBusy / (availability.length * totalMinutesPerAttendee)
  }

  function periodOffsetPx(p: AttendeeBusyPeriod, day: Date): { top: number; height: number } {
    const dayStart = stripTime(day).getTime()
    const dayEnd = dayStart + 24 * 60 * 60 * 1000
    const ps = Math.max(new Date(p.start).getTime(), dayStart)
    const pe = Math.min(new Date(p.end).getTime(), dayEnd)
    const top = ((ps - dayStart) / (dayEnd - dayStart)) * GRID_PX
    const height = Math.max(2, ((pe - ps) / (dayEnd - dayStart)) * GRID_PX)
    return { top, height }
  }

  function pickedSlotBand(): { top: number; height: number } | null {
    if (!sameDay(pickedStart, focusDay)) return null
    const dayStart = stripTime(focusDay).getTime()
    const dayEnd = dayStart + 24 * 60 * 60 * 1000
    const ps = Math.max(pickedStart.getTime(), dayStart)
    const pe = Math.min(pickedEnd.getTime(), dayEnd)
    if (pe <= ps) return null
    const top = ((ps - dayStart) / (dayEnd - dayStart)) * GRID_PX
    const height = Math.max(8, ((pe - ps) / (dayEnd - dayStart)) * GRID_PX)
    return { top, height }
  }

  function attendeeLabel(av: AttendeeAvailability): string {
    return av.displayName?.trim() || av.email
  }

  /** Tooltip for a busy period — only shows the time range, never
   *  the source event's title (free-busy responses don't carry
   *  details and the local-cache fallback would leak our own event
   *  titles otherwise). */
  function periodTooltip(p: AttendeeBusyPeriod): string {
    const fmt = new Intl.DateTimeFormat(undefined, {
      hour: '2-digit',
      minute: '2-digit',
    })
    return `${fmt.format(new Date(p.start))} – ${fmt.format(new Date(p.end))}`
  }

  function busyClasses(kind: AttendeeBusyPeriod['kind']): string {
    switch (kind) {
      case 'tentative':
        return 'bg-warning-500/40 border-warning-500/60'
      case 'unavailable':
        return 'bg-surface-700/60 border-surface-700/80 dark:bg-surface-300/60 dark:border-surface-300/80'
      case 'free':
        return 'bg-success-500/20 border-success-500/40'
      case 'busy':
      default:
        return 'bg-error-500/40 border-error-500/60'
    }
  }

  // ── Date formatters ──────────────────────────────────────────
  const dayLabelFmt = new Intl.DateTimeFormat(undefined, {
    weekday: 'short',
    day: 'numeric',
  })
  const fullDayFmt = new Intl.DateTimeFormat(undefined, {
    weekday: 'long',
    month: 'long',
    day: 'numeric',
  })
  const timeFmt = new Intl.DateTimeFormat(undefined, {
    hour: '2-digit',
    minute: '2-digit',
  })

  // 24 hourly labels: "00", "01", ..., "23".
  const HOURS = Array.from({ length: 24 }, (_, h) => h)

  function fmtHour(h: number): string {
    return h.toString().padStart(2, '0')
  }

  // Esc closes the modal.
  $effect(() => {
    if (!open) return
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    document.addEventListener('keydown', handler)
    return () => document.removeEventListener('keydown', handler)
  })
</script>

{#if open}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    onclick={close}
    role="presentation"
  >
    <div
      class="w-[860px] max-w-[95vw] max-h-[90vh] bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl flex flex-col"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      role="dialog"
      aria-modal="true"
      tabindex="-1"
    >
      <!-- Header -->
      <header class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center justify-between gap-3">
        <h2 class="text-base font-semibold shrink-0">{m.event_planner_title()}</h2>
        <button
          class="text-surface-500 hover:text-surface-900 dark:hover:text-surface-100"
          onclick={close}
          aria-label={m.event_planner_close()}
        >✕</button>
      </header>

      <!-- Scrub strip: 7 days ±3 around the focused day -->
      <section class="px-5 py-3 border-b border-surface-200 dark:border-surface-700">
        <div class="flex items-center justify-between mb-2">
          <span class="text-sm font-medium">{fullDayFmt.format(focusDay)}</span>
          {#if loading}
            <span class="text-xs text-surface-500">{m.event_planner_loading()}</span>
          {/if}
        </div>
        <div class="grid grid-cols-7 gap-1">
          {#each weekDays as day (day.getTime())}
            {@const focused = sameDay(day, focusDay)}
            {@const density = aggregateBusy(day)}
            <button
              type="button"
              class="rounded-md px-2 py-2 text-xs flex flex-col items-center gap-1 transition border {focused
                ? 'border-primary-500 bg-primary-500/10'
                : 'border-surface-200 dark:border-surface-700 hover:bg-surface-100 dark:hover:bg-surface-800'}"
              onclick={() => focusDifferentDay(day)}
            >
              <span class="font-medium">{dayLabelFmt.format(day)}</span>
              <!-- Busy density bar.  `density` is a fraction in
                   [0, 1]; we cap the bar's opacity so the cell
                   stays distinguishable from the focused style. -->
              <span
                class="h-1 w-full rounded-full bg-surface-300 dark:bg-surface-700 overflow-hidden"
              >
                <span
                  class="block h-full bg-error-500"
                  style="width: {Math.min(100, Math.round(density * 100))}%"
                ></span>
              </span>
            </button>
          {/each}
        </div>
      </section>

      <!-- Attendee × time grid -->
      <section class="flex-1 overflow-auto px-5 py-3">
        {#if attendees.length === 0}
          <p class="text-sm text-surface-500 italic">
            {m.event_planner_no_attendees()}
          </p>
        {:else if loadError}
          <p class="text-sm text-error-500">{loadError}</p>
        {:else}
          <!-- Grid container.  `relative` so the proposed-slot
               band absolutely positions against this row instead
               of falling through to the viewport.  `flex` (not
               inline-flex) + `min-w-full` + `flex-1` per attendee
               column means columns grow to fill the modal when
               there's room and only fall back to a per-column
               minimum (which triggers horizontal scroll) when
               the attendee count exceeds what fits. -->
          <div class="relative flex min-w-full">
            <!-- Time gutter on the left -->
            <div class="shrink-0" style="width: {TIME_GUTTER_PX}px">
              <!-- Header spacer aligning with attendee column header row -->
              <div class="h-8 border-b border-surface-200 dark:border-surface-700"></div>
              <div class="relative" style="height: {GRID_PX}px">
                {#each HOURS as h}
                  <div
                    class="absolute left-0 right-0 text-[10px] text-surface-500 pr-2 text-right"
                    style="top: {h * HOUR_PX - 6}px"
                  >
                    {h === 0 ? '' : fmtHour(h)}
                  </div>
                {/each}
              </div>
            </div>

            <!-- One column per attendee.  `flex-1` so columns
                 share whatever width is left after the gutter;
                 `min-w-[160px]` so a long attendee list still
                 produces a readable grid + horizontal scroll. -->
            {#each availability as av (av.email)}
              {@const dayPeriods = busyForDay(av.busyPeriods, focusDay)}
              <div
                class="flex-1 border-l border-surface-200 dark:border-surface-700"
                style="min-width: {ATTENDEE_COL_PX}px"
              >
                <!-- Attendee header -->
                <div
                  class="h-8 border-b border-surface-200 dark:border-surface-700 px-2 py-1 flex items-center justify-between gap-2"
                  title={av.email}
                >
                  <span class="truncate text-xs font-medium">{attendeeLabel(av)}</span>
                  {#if av.source === 'unknown'}
                    <span
                      class="shrink-0 text-[10px] text-surface-500"
                      title={m.event_planner_source_unknown_title()}
                    >?</span>
                  {:else if av.source === 'local-cache'}
                    <span
                      class="shrink-0 text-[10px] text-surface-500"
                      title={m.event_planner_source_local_title()}
                    >·</span>
                  {/if}
                </div>
                <!-- The clickable time grid -->
                <div
                  class="relative cursor-crosshair"
                  style="height: {GRID_PX}px"
                  onclick={gridClick}
                  onkeydown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault()
                      // Without an explicit click coordinate the
                      // keyboard activation puts the slot at noon
                      // — a reasonable neutral default.
                      const next = new Date(focusDay)
                      next.setHours(12, 0, 0, 0)
                      pickedStart = next
                      pickedEnd = new Date(next.getTime() + eventDurationMs)
                    }
                  }}
                  role="button"
                  tabindex="0"
                  aria-label={attendeeLabel(av)}
                >
                  <!-- Hour rows -->
                  {#each HOURS as h}
                    <div
                      class="absolute left-0 right-0 border-t border-surface-100 dark:border-surface-800"
                      style="top: {h * HOUR_PX}px; height: {HOUR_PX}px"
                    ></div>
                  {/each}
                  <!-- Busy blocks -->
                  {#each dayPeriods as p, i (i)}
                    {@const off = periodOffsetPx(p, focusDay)}
                    <div
                      class="absolute left-1 right-1 rounded-sm border {busyClasses(p.kind)}"
                      style="top: {off.top}px; height: {off.height}px"
                      title={periodTooltip(p)}
                    ></div>
                  {/each}
                </div>
              </div>
            {/each}

            <!-- Proposed slot — absolutely positioned against the
                 grid container above (which is `relative`).  Top
                 offset = the 32 px header row + the band's offset
                 inside the grid; `left` and `right` stretch the
                 band across all attendee columns regardless of
                 their flex-derived width.  `pointer-events-none`
                 so clicks pass through to the columns underneath. -->
            {#if pickedSlotBand()}
              {@const band = pickedSlotBand()!}
              <div
                class="pointer-events-none absolute border-2 border-primary-500 bg-primary-500/10 rounded-sm"
                style="
                  top: {32 + band.top}px;
                  left: {TIME_GUTTER_PX}px;
                  right: 0;
                  height: {band.height}px;
                "
              ></div>
            {/if}
          </div>
        {/if}
      </section>

      <!-- Footer with proposed-time summary + actions -->
      <footer class="px-5 py-3 border-t border-surface-200 dark:border-surface-700 flex items-center gap-3">
        <span class="text-sm text-surface-700 dark:text-surface-200">
          {m.event_planner_proposed_label()}: {fullDayFmt.format(pickedStart)},
          {timeFmt.format(pickedStart)} – {timeFmt.format(pickedEnd)}
        </span>
        <div class="flex-1"></div>
        <button class="btn preset-outlined-surface-500" onclick={close}>
          {m.event_planner_cancel()}
        </button>
        <button class="btn preset-filled-primary-500" onclick={apply}>
          {m.event_planner_apply()}
        </button>
      </footer>
    </div>
  </div>
{/if}
