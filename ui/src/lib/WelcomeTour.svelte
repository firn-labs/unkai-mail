<script lang="ts">
  // Guided welcome tour (#420) — a coach-mark overlay that walks a
  // new user through the app's key surfaces right after first-run
  // setup (and on demand from Settings → General).
  //
  // How targeting works: shell elements worth highlighting carry a
  // `data-tour="<key>"` attribute (Sidebar's Compose button, the
  // IconRail nav icons, the mail-list column, …).  Each step names
  // one of those keys; on entry we query *all* matching elements
  // and spotlight the union of their bounding boxes — that's what
  // lets one step frame the whole IconRail integration group even
  // though its buttons are loose flex children with no wrapper.
  //
  // Steps whose target isn't on screen (e.g. the Nextcloud nav
  // when no NC account is connected yet, or an anchor that moves
  // in a future redesign) degrade to a centered card instead of
  // pointing at nothing — the tour never hard-depends on a DOM
  // shape, so a stale anchor is a cosmetic downgrade, not a crash.
  //
  // The spotlight itself is the classic cutout trick: one absolutely
  // positioned div sitting on the target with a huge box-shadow
  // painting the dim layer, so the highlighted region stays at full
  // brightness without any compositing work.  The root div covers
  // the viewport and swallows pointer events, making the app inert
  // while the tour runs (same modality contract as our dialogs).
  import { anchorRect, visualToLayoutRatio } from './coords'
  import { m } from '../paraglide/messages'

  interface Props {
    /** Fired when the tour ends — finished *or* skipped.  The
        caller records completion either way ("skippable at any
        step; remembers completion"). */
    onclose: () => void
  }
  let { onclose }: Props = $props()

  interface TourStep {
    /** `data-tour` key to spotlight; `null` renders a centered card. */
    anchor: string | null
    title: () => string
    body: () => string
  }

  const STEPS: TourStep[] = [
    { anchor: null, title: m.tour_welcome_title, body: m.tour_welcome_body },
    { anchor: 'mail-list', title: m.tour_mail_list_title, body: m.tour_mail_list_body },
    { anchor: 'mail-list', title: m.tour_triage_title, body: m.tour_triage_body },
    { anchor: 'search', title: m.tour_search_title, body: m.tour_search_body },
    { anchor: 'compose', title: m.tour_compose_title, body: m.tour_compose_body },
    { anchor: 'rail-nav', title: m.tour_nextcloud_title, body: m.tour_nextcloud_body },
    { anchor: 'rail-settings', title: m.tour_settings_title, body: m.tour_settings_body },
    { anchor: null, title: m.tour_done_title, body: m.tour_done_body },
  ]

  let index = $state(0)
  const step = $derived(STEPS[index])

  let winW = $state(window.innerWidth)
  let winH = $state(window.innerHeight)

  interface Rect {
    top: number
    left: number
    width: number
    height: number
  }
  /** Spotlight box for the current step, `null` → centered card. */
  let rect = $state<Rect | null>(null)

  /** Breathing room between the target's edge and the cutout. */
  const SPOT_PAD = 6
  /** Gap between the spotlight and the tooltip card. */
  const MARGIN = 12

  function measure() {
    const anchor = STEPS[index].anchor
    if (!anchor) {
      rect = null
      return
    }
    const els = document.querySelectorAll(`[data-tour="${anchor}"]`)
    let top = Infinity
    let left = Infinity
    let right = -Infinity
    let bottom = -Infinity
    let found = false
    for (const el of els) {
      // Layout-space rect: the spotlight div lives inside the
      // zoomed root, so a raw visual-space rect would drift off
      // the target under the UI-scale zoom (#191).
      const r = anchorRect(el as HTMLElement)
      // display:none / unmounted-but-queried elements measure 0×0 —
      // they'd drag the union box to the viewport origin.
      if (r.width === 0 && r.height === 0) continue
      found = true
      top = Math.min(top, r.top)
      left = Math.min(left, r.left)
      right = Math.max(right, r.right)
      bottom = Math.max(bottom, r.bottom)
    }
    if (!found) {
      rect = null
      return
    }
    rect = {
      top: top - SPOT_PAD,
      left: left - SPOT_PAD,
      width: right - left + SPOT_PAD * 2,
      height: bottom - top + SPOT_PAD * 2,
    }
  }

  $effect(() => {
    // Re-measure when the step changes or the window resizes.  The
    // rAF defers past the current layout flush so a target that
    // mounted in the same tick (tour opening right as the inbox
    // renders) is measured after it has real geometry.
    void index
    void winW
    void winH
    const raf = requestAnimationFrame(measure)
    return () => cancelAnimationFrame(raf)
  })

  // Live card size for placement math; the bindings below keep
  // these honest once the card renders, the defaults only cover
  // the first frame.
  let cardW = $state(320)
  let cardH = $state(200)

  /** Tooltip position next to the spotlight: try right → below →
      above → left, then clamp into the viewport.  `null` while the
      step has no target (centered layout takes over). */
  const cardPos = $derived.by(() => {
    if (!rect) return null
    const r = rect
    // `winW`/`winH` are visual pixels (svelte:window bindings, kept
    // raw so resizes stay reactive); the placement math runs in
    // layout space alongside `rect` and the card's clientWidth.
    const ratio = visualToLayoutRatio()
    const vw = winW * ratio
    const vh = winH * ratio
    let top: number
    let left: number
    if (r.left + r.width + MARGIN + cardW <= vw) {
      left = r.left + r.width + MARGIN
      top = r.top
    } else if (r.top + r.height + MARGIN + cardH <= vh) {
      left = r.left
      top = r.top + r.height + MARGIN
    } else if (r.top - MARGIN - cardH >= 0) {
      left = r.left
      top = r.top - MARGIN - cardH
    } else {
      left = r.left - MARGIN - cardW
      top = r.top
    }
    left = Math.min(Math.max(MARGIN, left), Math.max(MARGIN, vw - cardW - MARGIN))
    top = Math.min(Math.max(MARGIN, top), Math.max(MARGIN, vh - cardH - MARGIN))
    return { top, left }
  })

  function next() {
    if (index < STEPS.length - 1) index++
    else onclose()
  }
  function back() {
    if (index > 0) index--
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault()
      onclose()
    } else if (e.key === 'ArrowRight') {
      e.preventDefault()
      next()
    } else if (e.key === 'ArrowLeft') {
      e.preventDefault()
      back()
    }
  }
</script>

<svelte:window bind:innerWidth={winW} bind:innerHeight={winH} onkeydown={onKeydown} />

{#snippet cardContent()}
  <p class="text-xs text-surface-400 dark:text-surface-500 mb-1">
    {m.tour_step_of({ n: index + 1, total: STEPS.length })}
  </p>
  <h3 class="text-base font-semibold mb-2">{step.title()}</h3>
  <p class="text-sm text-surface-600 dark:text-surface-300">{step.body()}</p>
  <div class="flex items-center gap-2 mt-4">
    {#if index < STEPS.length - 1}
      <button class="btn btn-sm preset-outlined-surface-500" onclick={onclose}>
        {m.tour_skip()}
      </button>
    {/if}
    <div class="flex-1"></div>
    {#if index > 0}
      <button class="btn btn-sm preset-outlined-surface-500" onclick={back}>
        {m.tour_back()}
      </button>
    {/if}
    <button class="btn btn-sm preset-filled-primary-500" onclick={next}>
      {index === STEPS.length - 1 ? m.tour_finish() : m.tour_next()}
    </button>
  </div>
{/snippet}

<!-- z-[70] sits above the app's confirm modals (z-60) so the tour
     can never end up spotlighting from underneath one. -->
<div class="fixed inset-0 z-[70]" role="dialog" aria-modal="true" aria-label={m.tour_welcome_title()}>
  {#if rect}
    <div
      class="absolute rounded-lg pointer-events-none ring-2 ring-primary-500"
      style="top:{rect.top}px; left:{rect.left}px; width:{rect.width}px; height:{rect.height}px;
             box-shadow: 0 0 0 9999px rgb(0 0 0 / 0.55);
             transition: top 0.25s ease, left 0.25s ease, width 0.25s ease, height 0.25s ease;"
      aria-hidden="true"
    ></div>
  {:else}
    <div class="absolute inset-0 bg-black/55" aria-hidden="true"></div>
  {/if}

  {#if cardPos}
    <div
      class="absolute w-80 glass-float rounded-2xl p-4"
      style="top:{cardPos.top}px; left:{cardPos.left}px; transition: top 0.25s ease, left 0.25s ease;"
      bind:clientWidth={cardW}
      bind:clientHeight={cardH}
    >
      {@render cardContent()}
    </div>
  {:else}
    <div class="absolute inset-0 flex items-center justify-center pointer-events-none">
      <div
        class="w-96 max-w-[90%] glass-float rounded-2xl p-5 pointer-events-auto"
      >
        {@render cardContent()}
      </div>
    </div>
  {/if}
</div>
