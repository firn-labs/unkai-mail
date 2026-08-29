<script lang="ts">
  /**
   * FullscreenState — the app's ONE full-viewport wait screen
   * (#536).  Two variants from one shell:
   *
   *   - **Boot / loading** (no `toIcon`): the app logo with a calm
   *     pulse and a status line — replaces the bare "Starting
   *     up…" / "Loading..." text divs the shell used to flash.
   *   - **Profile transition** (`toIcon` set): the outgoing
   *     profile's glyph on the left, the incoming one ringed on
   *     the right, animated "data flow" dots between them.  Shown
   *     by `App.svelte` for the whole switch-in-place sequence —
   *     registry remap → store reloads → first paint (#535/#536).
   *
   * Shell rules: `fixed inset-0` at `z-[1000]` (the `LockScreen`
   * precedent — top of the z-ladder), theme-token backgrounds,
   * `role="dialog" aria-modal="true"` (the `WelcomeTour`
   * precedent).  The dismissal fade is the caller removing the
   * component — the flow animation loops and never gates progress;
   * under `prefers-reduced-motion` only the fades remain.
   */
  import { fade } from 'svelte/transition'
  import * as api from './api'
  import type { ProfileIcon } from './api'
  import ProfileGlyph from './ProfileGlyph.svelte'

  interface Props {
    /** Status line under the artwork (localised by the caller). */
    label: string
    /** Transition variant: the profile being switched away from.
     *  Optional — a window whose profile is still resolving shows
     *  the app logo on the outgoing side instead. */
    fromIcon?: ProfileIcon | null
    /** Transition variant trigger: the profile being switched to. */
    toIcon?: ProfileIcon | null
    /** Logo style for the boot variant / the fallback from-side —
     *  'storm' until the profile's settings are readable. */
    logoStyle?: string
  }
  let { label, fromIcon = null, toIcon = null, logoStyle = 'storm' }: Props = $props()

  const logoUrl = $derived(api.platform.assetUrl(logoStyle, 'unkai-logo'))
</script>

<!-- `|global`: the component is mounted/removed by the parent's
     `{#if}` branches, and a local transition would only play for
     blocks owned by this component. -->
<div
  class="fixed inset-0 z-[1000] flex items-center justify-center bg-surface-50 dark:bg-surface-900"
  role="dialog"
  aria-modal="true"
  aria-label={label}
  transition:fade|global={{ duration: 150 }}
>
  <div class="flex flex-col items-center gap-8">
    {#if toIcon}
      <div class="flex items-center gap-8">
        <!-- Outgoing side, dimmed — the profile the window is
             leaving behind. -->
        <div
          class="w-20 h-20 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300
                 flex items-center justify-center opacity-60"
        >
          {#if fromIcon}
            <ProfileGlyph icon={fromIcon} size={40} />
          {:else}
            <img src={logoUrl} alt="" class="w-10 h-10" draggable="false" />
          {/if}
        </div>
        <!-- Data-flow motion: dots travelling old → new along a
             faint track.  Pure decoration (aria-hidden); hidden
             entirely under prefers-reduced-motion. -->
        <div class="flow-track relative w-28 h-1 rounded-full" aria-hidden="true">
          <span class="flow-dot" style="animation-delay: 0s"></span>
          <span class="flow-dot" style="animation-delay: 0.45s"></span>
          <span class="flow-dot" style="animation-delay: 0.9s"></span>
        </div>
        <!-- Incoming side, ringed — where the window is heading. -->
        <div
          class="w-20 h-20 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300
                 flex items-center justify-center ring-2 ring-primary-500/50
                 ring-offset-4 ring-offset-surface-50 dark:ring-offset-surface-900"
        >
          <ProfileGlyph icon={toIcon} size={40} />
        </div>
      </div>
    {:else}
      <!-- Boot / loading: the single-icon variant.  motion-safe
           keeps the pulse off for reduced-motion users. -->
      <img
        src={logoUrl}
        alt=""
        class="w-20 h-20 motion-safe:animate-pulse"
        draggable="false"
      />
    {/if}
    <p class="text-sm text-surface-500" aria-live="polite">{label}</p>
  </div>
</div>

<style>
  /* Track + dots use the house color-mix-over-theme-token style —
     theme colours are oklch and are never hand-parsed. */
  .flow-track {
    background: color-mix(in oklab, var(--color-primary-500) 15%, transparent);
  }
  .flow-dot {
    position: absolute;
    left: -0.25rem;
    top: 50%;
    width: 0.5rem;
    height: 0.5rem;
    margin-top: -0.25rem;
    border-radius: 9999px;
    background: color-mix(in oklab, var(--color-primary-500) 85%, transparent);
    opacity: 0;
    animation: flow 1.4s ease-in-out infinite;
  }
  /* The hero flow motion — deliberately longer than the house
     150ms micro-cadence; it loops and never gates dismissal. */
  @keyframes flow {
    0% {
      transform: translateX(0) scale(0.6);
      opacity: 0;
    }
    20% {
      opacity: 1;
    }
    80% {
      opacity: 1;
    }
    100% {
      transform: translateX(7rem) scale(0.6);
      opacity: 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    /* Fade-only: the travelling dots disappear, the faint track
       stays as a static "these two are connected" hint. */
    .flow-dot {
      display: none;
    }
  }
</style>
