<script lang="ts" module>
  /**
   * Badge — the one pill shape (docs/UI_CONVENTIONS.md rule 9).
   *
   * Every rounded-full status/label pill in the app renders through
   * this component so the shape, type treatment, and tone palette
   * stay identical everywhere: 10px uppercase semibold on a subtle
   * alpha tint of a Skeleton semantic token, optional leading icon.
   * Don't inline a new pill variant — add a tone here if a genuinely
   * new semantic appears, and update the conventions doc first.
   */
  export type BadgeTone =
    | 'neutral'
    | 'primary'
    | 'secondary'
    | 'tertiary'
    | 'success'
    | 'warning'
    | 'error'

  export const BADGE_TONE_CLASSES: Record<BadgeTone, string> = {
    neutral:
      'bg-surface-300/40 text-surface-700 dark:bg-surface-700/40 dark:text-surface-300',
    primary: 'bg-primary-500/15 text-primary-700 dark:text-primary-300',
    secondary: 'bg-secondary-500/15 text-secondary-700 dark:text-secondary-300',
    tertiary: 'bg-tertiary-500/15 text-tertiary-700 dark:text-tertiary-300',
    success: 'bg-success-500/15 text-success-700 dark:text-success-300',
    warning: 'bg-warning-500/15 text-warning-700 dark:text-warning-300',
    error: 'bg-error-500/15 text-error-600 dark:text-error-400',
  }
</script>

<script lang="ts">
  import Icon, { type IconName } from './Icon.svelte'

  interface Props {
    label: string
    tone?: BadgeTone
    /** Optional leading icon from the registry, rendered at 11px. */
    icon?: IconName
    /** Tooltip + aria-label for badges whose label alone is terse
     *  (e.g. "PGP" → "Encrypted with OpenPGP"). */
    title?: string
    /** Extra classes (margins / alignment flags only). */
    class?: string
  }
  let { label, tone = 'neutral', icon, title, class: cls = '' }: Props = $props()
</script>

<span
  class="inline-flex items-center gap-1 rounded-full px-2 py-[1px] text-[10px] font-semibold uppercase tracking-wide leading-tight align-middle {BADGE_TONE_CLASSES[
    tone
  ]} {cls}"
  {title}
  aria-label={title}
>
  {#if icon}<Icon name={icon} size={11} />{/if}
  <span>{label}</span>
</span>
