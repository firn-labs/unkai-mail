<script lang="ts">
  /**
   * LocationField — replaces the plain `<input>` on the event
   * editor's Location row (#280).
   *
   * Behaviour:
   *   - Free-text typing still works: every keystroke binds back
   *     to the parent's `value` prop the same way an `<input>`
   *     would.  Geocoding never blocks the keystroke flow.
   *   - 350ms after the user stops typing, the field calls
   *     `geocode_search` (cache-aware Tauri command) and renders
   *     the result list as a dropdown beneath the input.
   *   - Picking a suggestion replaces `value` with the canonical
   *     `display_name` and stamps `(latitude, longitude)` onto
   *     the bound parent state via the `onpick` callback.  The
   *     parent keeps these next to the `LOCATION` field on its
   *     `CalendarEventInput` so the GEO property round-trips.
   *   - Manually editing `value` after a pick clears the bound
   *     `(lat, lon)` — a free-text address that doesn't match
   *     the geocoded one shouldn't keep the old pin around.
   *
   * The dropdown follows the project's outside-click idiom:
   * register the dismiss listener inside an `$effect` keyed on
   * the open state, with a one-tick delay so the click that
   * opened it doesn't immediately close it.
   */
  import { invoke } from '@tauri-apps/api/core'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  interface GeocodeResult {
    placeId: number
    displayName: string
    lat: number
    lon: number
    osmType?: string | null
    class?: string | null
    kind?: string | null
  }

  interface Props {
    value: string
    latitude: number | null
    longitude: number | null
    placeholder?: string
    /** IETF tag for Nominatim's Accept-Language header.  Empty
     *  string keeps the server-default (local-language names). */
    lang?: string
    /** Fired when the user picks a suggestion or clears the
     *  geocoded match by editing free-form. */
    onpick: (
      value: string,
      latitude: number | null,
      longitude: number | null,
    ) => void
  }

  let {
    value,
    latitude,
    longitude,
    placeholder = '',
    lang = '',
    onpick,
  }: Props = $props()

  let suggestions = $state<GeocodeResult[]>([])
  let open = $state(false)
  let loading = $state(false)
  let activeIndex = $state(-1)
  let inputEl = $state<HTMLInputElement | null>(null)
  let listEl = $state<HTMLDivElement | null>(null)

  // ── Debounced fetch ─────────────────────────────────────────
  // 350ms matches the user's chosen pacing (privacy + Nominatim's
  // 1 req/sec cap).  Clearing on every keystroke means a slow
  // typist gets a single request per pause.
  let debounceTimer: ReturnType<typeof setTimeout> | null = null

  function scheduleFetch(query: string) {
    if (debounceTimer) clearTimeout(debounceTimer)
    if (query.trim().length < 2) {
      suggestions = []
      open = false
      return
    }
    debounceTimer = setTimeout(() => {
      void runFetch(query.trim())
    }, 350)
  }

  async function runFetch(query: string) {
    loading = true
    try {
      const hits = await invoke<GeocodeResult[]>('geocode_search', {
        query,
        lang,
      })
      suggestions = hits
      open = hits.length > 0
      activeIndex = hits.length > 0 ? 0 : -1
    } catch (e) {
      console.warn('geocode_search failed', e)
      suggestions = []
      open = false
    } finally {
      loading = false
    }
  }

  // ── Input handlers ──────────────────────────────────────────
  function onInput(e: Event) {
    const next = (e.target as HTMLInputElement).value
    // The user typed manually — if there was a geocoded pin and
    // the new text no longer matches the picked display name,
    // drop the lat/lon so the map doesn't lie.
    if ((latitude !== null || longitude !== null) && next !== value) {
      onpick(next, null, null)
    } else {
      onpick(next, latitude, longitude)
    }
    scheduleFetch(next)
  }

  function pick(hit: GeocodeResult) {
    onpick(hit.displayName, hit.lat, hit.lon)
    open = false
    suggestions = []
    activeIndex = -1
    // Return focus to the input so the next Tab key works
    // intuitively for keyboard-only users.
    inputEl?.focus()
  }

  function onKeyDown(e: KeyboardEvent) {
    if (!open || suggestions.length === 0) {
      // Escape always works to clear the dropdown even when it
      // hasn't opened yet — harmless, helps with stale UI.
      if (e.key === 'Escape') open = false
      return
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      activeIndex = Math.min(activeIndex + 1, suggestions.length - 1)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      activeIndex = Math.max(activeIndex - 1, 0)
    } else if (e.key === 'Enter') {
      if (activeIndex >= 0 && activeIndex < suggestions.length) {
        e.preventDefault()
        pick(suggestions[activeIndex])
      }
    } else if (e.key === 'Escape') {
      open = false
    }
  }

  // ── Outside-click dismissal ─────────────────────────────────
  $effect(() => {
    if (!open) return
    const handler = (e: MouseEvent) => {
      const target = e.target as Node | null
      if (target && (inputEl?.contains(target) || listEl?.contains(target))) return
      open = false
    }
    // One-tick delay so the click that opened the dropdown
    // doesn't immediately dismiss it.
    const id = setTimeout(() => document.addEventListener('mousedown', handler), 0)
    return () => {
      clearTimeout(id)
      document.removeEventListener('mousedown', handler)
    }
  })
</script>

<div class="relative flex-1">
  <input
    bind:this={inputEl}
    id="event-location"
    class="input w-full px-3 py-2 text-sm rounded-md"
    {value}
    {placeholder}
    aria-label={placeholder || 'Location'}
    aria-autocomplete="list"
    aria-expanded={open}
    autocomplete="off"
    oninput={onInput}
    onkeydown={onKeyDown}
    onfocus={() => {
      if (suggestions.length > 0) open = true
    }}
  />

  {#if open}
    <div
      bind:this={listEl}
      class="absolute left-0 right-0 top-full mt-1 z-50 max-h-72 overflow-y-auto bg-surface-50 dark:bg-surface-900 border border-surface-300 dark:border-surface-700 rounded-md shadow-lg"
      role="listbox"
    >
      {#if loading && suggestions.length === 0}
        <div class="px-3 py-2 text-sm text-surface-500">
          {m.location_field_loading()}
        </div>
      {:else}
        {#each suggestions as hit, i (hit.placeId)}
          <button
            type="button"
            role="option"
            aria-selected={i === activeIndex}
            class="w-full text-left flex items-start gap-2 px-3 py-2 text-sm cursor-pointer {i === activeIndex
              ? 'bg-primary-500/15'
              : 'hover:bg-surface-100 dark:hover:bg-surface-800'}"
            onmousedown={(e) => {
              // Stop the document-level mousedown listener that
              // dismisses the popover from firing first.
              e.preventDefault()
              e.stopPropagation()
              pick(hit)
            }}
            onmouseenter={() => {
              activeIndex = i
            }}
          >
            <Icon name="search" size={14} class="mt-0.5 shrink-0 text-surface-500" />
            <span class="flex-1 min-w-0">
              <span class="block truncate">{hit.displayName}</span>
              {#if hit.kind || hit.class}
                <span class="block text-[10px] text-surface-500 truncate">
                  {hit.kind || hit.class}
                </span>
              {/if}
            </span>
          </button>
        {/each}
      {/if}
    </div>
  {/if}
</div>
