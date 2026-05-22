<script lang="ts" module>
  /**
   * Tailwind palette tuned for Skeleton's cerberus theme.  Hash-indexed
   * by sender so threads from one person carry a stable colour across
   * views.  Each entry is a (bg, text) pair so contrast survives both
   * light and dark mode.  Keep this in the module scope (not a closure)
   * so the palette is a single shared array — every `<Avatar>` instance
   * hashes against the same six buckets.
   */
  const PALETTE = [
    'bg-primary-500/20 text-primary-700 dark:text-primary-300',
    'bg-secondary-500/20 text-secondary-700 dark:text-secondary-300',
    'bg-tertiary-500/20 text-tertiary-700 dark:text-tertiary-300',
    'bg-success-500/20 text-success-700 dark:text-success-300',
    'bg-warning-500/20 text-warning-700 dark:text-warning-300',
    'bg-error-500/20 text-error-700 dark:text-error-300',
  ]

  /** djb2-ish 32-bit hash — deterministic, no external deps, plenty of
   *  spread for the 6-bucket palette.  Coerced into a positive index
   *  via `Math.abs` because the `|0` cast above can land negative. */
  function hashIndex(s: string, modulo: number): number {
    if (!s) return 0
    let h = 0
    for (let i = 0; i < s.length; i++) {
      h = (h * 31 + s.charCodeAt(i)) | 0
    }
    return Math.abs(h) % modulo
  }

  export function avatarColourClass(seed: string): string {
    return PALETTE[hashIndex(seed, PALETTE.length)]
  }
</script>

<script lang="ts">
  /**
   * Avatar — round photo or initials circle.  Used by ContactsView
   * (contact list rows, mailing-list member rows) and MailList (the
   * sender column on each envelope row, #305).  Centralising the
   * markup here keeps the two views visually identical without each
   * having to repeat the photo/initials branching.
   *
   * The size is driven via inline `width` / `height` style (not a
   * Tailwind `w-N h-N` class) so a numeric prop survives Tailwind's
   * just-in-time class scanner — dynamically composed utility class
   * names get purged out of the final CSS bundle.
   */
  import { nameInitials } from './fromHeader'

  interface Props {
    /** Pre-resolved photo URL (e.g. via `contactPhotoSrc`).  Pass
     *  null/undefined to render the initials fallback. */
    photo?: string | null
    /** String the initial letter is taken from.  Required so we
     *  always have *something* to render even when the photo URL is
     *  missing or 404s mid-flight. */
    displayName: string
    /** Stable seed for the colour-hash bucket.  Defaults to the
     *  display name so two unknown senders still get different
     *  colours; pass the email when you have it so two contacts with
     *  the same first letter remain distinguishable. */
    seed?: string
    /** Pixel size — used for both width/height and the font-size of
     *  the initial letter (scaled to ~40% of the box).  32 matches
     *  the contact-row layout; mail rows use 36 for visual balance
     *  against the two-line text block. */
    size?: number
    /** Optional `<img alt>` text.  Defaults to empty (decorative) —
     *  the sender's name is already rendered next to the avatar, so
     *  duplicating it would just produce a screen-reader stutter. */
    alt?: string
  }

  const {
    photo,
    displayName,
    seed,
    size = 32,
    alt = '',
  }: Props = $props()

  // Two-letter initials when the name has more than one word
  // ("Max Mustermann" → "MM"), single letter otherwise.  See
  // `nameInitials` in fromHeader.ts for the full rule table.
  const initials = $derived(nameInitials(displayName))

  const colourClass = $derived(avatarColourClass(seed ?? displayName ?? ''))
  const sizePx = $derived(`${size}px`)
  // Font scales down a notch when there are two glyphs to fit so the
  // pair doesn't crowd the circle.  Floor at 9 px so the smallest
  // (28 px) avatars stay legible.
  const fontPx = $derived(
    `${Math.max(9, Math.round(size * (initials.length > 1 ? 0.36 : 0.42)))}px`,
  )
</script>

{#if photo}
  <img
    src={photo}
    {alt}
    loading="lazy"
    class="rounded-full object-cover shrink-0"
    style="width: {sizePx}; height: {sizePx};"
  />
{:else}
  <span
    class="rounded-full font-semibold flex items-center justify-center shrink-0 {colourClass}"
    style="width: {sizePx}; height: {sizePx}; font-size: {fontPx};"
    aria-hidden="true"
  >
    {initials}
  </span>
{/if}
