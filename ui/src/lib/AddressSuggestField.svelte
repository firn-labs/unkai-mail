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
  /** Generation counter so a slow in-flight fetch can't
   *  clobber a newer one's result.  Each scheduled fetch
   *  bumps it; `runFetch` only writes back if its captured
   *  generation still matches when the network call returns. */
  let fetchGen = 0

  function scheduleFetch(query: string) {
    if (debounceTimer) clearTimeout(debounceTimer)
    if (!enabled) {
      suggestions = []
      open = false
      return
    }
    const trimmed = query.trim()
    if (trimmed.length === 0) {
      // Empty input — fully reset.  Anything else (1-char
      // prefixes, sub-threshold strings) keeps whatever we
      // last had so the dropdown doesn't flicker shut while
      // the user is mid-edit.
      suggestions = []
      open = false
      return
    }
    if (trimmed.length < 2) {
      // Below the useful-query floor; don't fire a fetch but
      // also don't clear the previously-rendered suggestions.
      return
    }
    debounceTimer = setTimeout(() => {
      void runFetch(trimmed)
    }, 350)
  }

  async function runFetch(query: string) {
    const gen = ++fetchGen
    loading = true
    try {
      const hits = await invoke<GeocodeResult[]>('geocode_search', {
        query,
        lang,
      })
      // A newer fetch already started — drop this stale
      // result so we don't paint the user's old query over
      // their newer one.
      if (gen !== fetchGen) return
      if (hits.length > 0) {
        suggestions = hits
        open = true
        activeIndex = 0
      } else if (suggestions.length === 0) {
        // No hits *and* no previous results to fall back
        // on — leave the dropdown closed.
        open = false
      }
      // Else: keep the previously-rendered suggestions
      // visible.  This is what fixes the "type a house
      // number, dropdown disappears" bug — Nominatim
      // returns 0 hits for many partial street+number
      // combos, but the user's earlier "Schillerstraße"
      // suggestions are still the right pick to advance
      // the form.
    } catch (e) {
      console.warn('geocode_search (address) failed', e)
      // Don't blow away suggestions on a network blip —
      // they're still useful even if the next refinement
      // failed.
    } finally {
      if (gen === fetchGen) loading = false
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
   *  works for most European postal conventions including DE.
   *
   *  Fallback: a hit with no `address` block at all (rare,
   *  but Nominatim returns these for some POI / amenity hits)
   *  seeds `street` with the human-readable `display_name`
   *  rather than emptying the form silently — picking *any*
   *  suggestion should produce *something* visible. */
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
    if (!street && !locality && !region && !postal_code && !country) {
      return {
        street: hit.displayName,
        locality: '',
        region: '',
        postal_code: '',
        country: '',
      }
    }
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
    class="input rounded-lg w-full"
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
      class="absolute left-0 right-0 top-full mt-1 z-50 max-h-72 overflow-y-auto bg-surface-50 dark:bg-surface-900 border border-surface-300 dark:border-surface-700 rounded-lg shadow-lg"
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
              : 'hover:bg-primary-500/10'}"
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
