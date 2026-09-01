<script lang="ts">
  // Small coloured chip for vCard email-kind labels (HOME / WORK
  // / CELL / OTHER / …) shown next to an address in autocomplete
  // dropdowns.  Renders through the shared Badge pill so the chip
  // language reads consistently across the app; tones are Skeleton
  // theme tokens, so the chip re-tints when the user switches
  // themes.
  import Badge, { type BadgeTone } from './Badge.svelte'

  interface Props {
    /** vCard kind string — `HOME`, `WORK`, `CELL`, `OTHER`,
     *  `INTERNET`, etc.  Case-insensitive. */
    kind?: string | null
    /** Optional extra classes (margins / inline-block flags). */
    class?: string
  }
  let { kind = '', class: cls = '' }: Props = $props()

  const meta = $derived.by((): { label: string; tone: BadgeTone } | null => {
    const k = (kind ?? '').toLowerCase()
    if (!k) return null
    if (k.includes('work')) return { label: 'Work', tone: 'primary' }
    if (k.includes('home')) return { label: 'Home', tone: 'success' }
    if (k.includes('cell') || k.includes('mobile'))
      return { label: 'Mobile', tone: 'secondary' }
    if (k.includes('fax')) return { label: 'Fax', tone: 'warning' }
    if (k.includes('internet')) return { label: 'Internet', tone: 'tertiary' }
    return { label: kind ?? 'Other', tone: 'neutral' }
  })
</script>

{#if meta}
  <Badge label={meta.label} tone={meta.tone} class={cls} />
{/if}
