<script lang="ts">
  /**
   * MapPreview — read-only map of a single (lat, lon) pin (#280).
   *
   * Renders OpenStreetMap's official embed endpoint
   * (`/export/embed.html`) inside an `<iframe>`.  That endpoint
   * is a real interactive Leaflet view served by openstreetmap.org
   * with proper attribution baked in, so we get pan/zoom and a
   * styled marker without pulling MapLibre + a vector style file
   * into the bundle.
   *
   * Privacy: the iframe loads tiles + scripts from
   * openstreetmap.org.  No user identifiers are sent — the URL
   * carries only the bbox + marker coordinates.  The iframe
   * sandbox attributes block the embedded page from navigating
   * the host or running top-level scripts; clicks inside the
   * iframe stay on osm.org.
   */
  import { m } from '../paraglide/messages'

  interface Props {
    latitude: number
    longitude: number
    /** Half-width of the bounding box in degrees.  0.005 ≈ 550 m
     *  at the equator; small enough that a city-block address
     *  fills the frame, large enough that a place at the edge of
     *  a town is still readable. */
    halfDelta?: number
    /** Optional human label rendered as a caption above the map.
     *  We never inject the label into the iframe URL — the OSM
     *  embed has no "show this title" parameter — but the
     *  surrounding chrome benefits from the context. */
    caption?: string
  }

  let { latitude, longitude, halfDelta = 0.005, caption = '' }: Props = $props()

  let bbox = $derived.by(() => {
    const w = (longitude - halfDelta).toFixed(6)
    const s = (latitude - halfDelta).toFixed(6)
    const e = (longitude + halfDelta).toFixed(6)
    const n = (latitude + halfDelta).toFixed(6)
    return `${w},${s},${e},${n}`
  })

  let marker = $derived(`${latitude.toFixed(6)},${longitude.toFixed(6)}`)
  let embedUrl = $derived(
    `https://www.openstreetmap.org/export/embed.html?bbox=${bbox}&layer=mapnik&marker=${marker}`,
  )
  let openInOsmUrl = $derived(
    `https://www.openstreetmap.org/?mlat=${latitude.toFixed(6)}&mlon=${longitude.toFixed(6)}#map=16/${latitude.toFixed(6)}/${longitude.toFixed(6)}`,
  )
</script>

<div class="rounded-lg overflow-hidden border border-surface-200 dark:border-surface-700">
  {#if caption}
    <div class="px-3 py-2 text-xs text-surface-600 dark:text-surface-300 bg-surface-100 dark:bg-surface-800 truncate" title={caption}>
      {caption}
    </div>
  {/if}
  <!-- The iframe `sandbox` strips form submissions, top-level
       navigation, and same-origin access.  `allow-scripts`
       stays on so OSM's Leaflet renders interactively. -->
  <iframe
    title={m.map_preview_iframe_title()}
    src={embedUrl}
    class="w-full h-[220px] block bg-surface-100 dark:bg-surface-800"
    sandbox="allow-scripts"
    referrerpolicy="no-referrer"
    loading="lazy"
  ></iframe>
  <div class="flex items-center justify-between px-3 py-1 text-[10px] text-surface-500 bg-surface-100 dark:bg-surface-800">
    <span>{m.map_preview_attribution()}</span>
    <a
      href={openInOsmUrl}
      target="_blank"
      rel="noopener noreferrer"
      class="underline hover:text-primary-500"
    >
      {m.map_preview_open_external()}
    </a>
  </div>
</div>
