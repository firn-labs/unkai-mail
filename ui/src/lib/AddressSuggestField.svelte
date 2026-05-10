<script lang="ts">
  /**
   * AddressSuggestField — typeahead for the contact form's
   * Addresses section (#259).  Wraps the street input with a
   * suggestion dropdown that, on pick, fills street / locality /
   * region / postal_code / country in one shot via an `onpick`
   * callback.
   *
   * Reuses the `geocode_search` Tauri command (and the
   * `geocode_cache` table behind it) introduced for #280's
   * EventEditor location autocomplete.  The same privacy toggle
   * (`location_geocoding_enabled`) gates this component too —
   * when off, the field stays a plain `<input>` that never
   * touches the network.
   *
   * Same UX shape as LocationField:
   *   - 350 ms debounce after the last keystroke
   *   - Arrow Up / Down + Enter for keyboard pick
   *   - Outside-click / Escape dismisses the dropdown
   *
   * Output shape: `onpick` receives the five form fields
   * (`street`, `locality`, `region`, `postal_code`, `country`)
   * already mapped from Nominatim's structured `address` object,
   * so the parent doesn't need to know which Nominatim key wins
   * for which form field.
   */
  import { invoke } from '@tauri-apps/api/core'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  interface GeocodeAddress {
    road?: string | null
    houseNumber?: string | null
    neighbourhood?: string | null
    suburb?: string | null
    city?: string | null
    town?: string | null
    village?: string | null
    hamlet?: string | null
    municipality?: string | null
    county?: string | null
    state?: string | null
    stateDistrict?: string | null
    region?: string | null
    postcode?: string | null
    country?: string | null
    countryCode?: string | null
  }

  interface GeocodeResult {
    placeId: number
    displayName: string
    lat: number
    lon: number
    osmType?: string | null
    class?: string | null
    kind?: string | null
    address?: GeocodeAddress | null
  }

  /** What the parent gets back when the user picks a suggestion. */
  export interface AddressPick {
    street: string
    locality: string
    region: string
    postal_code: string
    country: string
  }

  interface Props {
    street: string
    placeholder?: string
    /** IETF tag for Nominatim's Accept-Language header.  Empty
     *  string keeps the server-default (local-language names). */
    lang?: string
    /** Privacy gate (#280).  When `false` the field stays a
     *  plain text input — no debounced fetch, no dropdown.
     *  Default `false`. */
    enabled?: boolean
    /** Fired on every keystroke; the parent updates its own
     *  `addr.street` so the input is fully controlled. */
    onstreetchange: (v: string) => void
    /** Fired when the user picks a suggestion.  The parent
     *  applies the structured parts in one go. */
    onpick: (parts: AddressPick) => void
  }

  let {
    street,
    placeholder = '',
    lang = '',
    enabled = false,
    onstreetchange,
    onpick,
  }: Props = $props()

  let suggestions = $state<GeocodeResult[]>([])
  let open = $state(false)
  let loading = $state(false)
  let activeIndex = $state(-1)
  let inputEl = $state<HTMLInputElement | null>(null)
  let listEl = $state<HTMLDivElement | null>(null)

  let debounceTimer: ReturnType<typeof setTimeout> | null = null

  function scheduleFetch(query: string) {
    if (debounceTimer) clearTimeout(debounceTimer)
    if (!enabled) {
      suggestions = []
      open = false
      return
    }
    if (query.trim().length < 3) {
      // Bumped to 3 (vs 2 in LocationField) because postal-
      // address queries with two-character prefixes return
      // junk hits (single-letter street abbreviations) that
      // pollute the dropdown.
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
      // Drop hits without an `address` block — those don't
      // have anything we could fill into the structured
      // form fields.  Nominatim emits these for some POI
      // types; for street-address geocoding we want the
      // ones that resolved to a postal address.
      suggestions = hits.filter((h) => !!h.address)
      open = suggestions.length > 0
      activeIndex = suggestions.length > 0 ? 0 : -1
    } catch (e) {
      console.warn('geocode_search (address) failed', e)
      suggestions = []
      open = false
    } finally {
      loading = false
    }
  }

  function onInput(e: Event) {
    const next = (e.target as HTMLInputElement).value
    onstreetchange(next)
    scheduleFetch(next)
  }

  /** Map a Nominatim `address` object to the form fields.
   *  Locality / region fall back through the OSM hierarchy
   *  because Nominatim only emits the keys it has — a German
   *  city sets `city`, a small village sets `village`, etc.
   *  Street combines `road` with `house_number`; ordering is
   *  language-dependent in real life, but `road house_number`
   *  works for most European postal conventions including DE. */
  function pickToForm(hit: GeocodeResult): AddressPick {
    const a = hit.address ?? ({} as GeocodeAddress)
    const street = [a.road ?? '', a.houseNumber ?? '']
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
      .join(' ')
    const locality =
      a.city
      || a.town
      || a.village
      || a.hamlet
      || a.municipality
      || a.suburb
      || a.neighbourhood
      || ''
    const region =
      a.state || a.stateDistrict || a.region || a.county || ''
    const postal_code = a.postcode || ''
    const country = a.country || ''
    return { street, locality, region, postal_code, country }
  }

  function pick(hit: GeocodeResult) {
    onpick(pickToForm(hit))
    open = false
    suggestions = []
    activeIndex = -1
    inputEl?.focus()
  }

  function onKeyDown(e: KeyboardEvent) {
    if (!open || suggestions.length === 0) {
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

  $effect(() => {
    if (!open) return
    const handler = (e: MouseEvent) => {
      const target = e.target as Node | null
      if (target && (inputEl?.contains(target) || listEl?.contains(target))) return
      open = false
    }
    const id = setTimeout(() => document.addEventListener('mousedown', handler), 0)
    return () => {
      clearTimeout(id)
      document.removeEventListener('mousedown', handler)
    }
  })
</script>

<div class="relative">
  <input
    bind:this={inputEl}
    class="input rounded-md w-full"
    value={street}
    {placeholder}
    aria-label={placeholder || 'Street'}
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
          {m.address_suggest_loading()}
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
            </span>
          </button>
        {/each}
        <!-- Nominatim's posted attribution requirement (#259). -->
        <div class="px-3 py-1 text-[10px] text-surface-500 border-t border-surface-200 dark:border-surface-700">
          {m.address_suggest_attribution()}
        </div>
      {/if}
    </div>
  {/if}
</div>
