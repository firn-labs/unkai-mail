<script lang="ts">
  /**
   * ShrunkenComposesBar — browser-style tab strip docked to the
   * bottom-right of the mail view for any Compose modals the user
   * has minimised (#292).
   *
   * Each tab carries:
   *   - the compose icon + the draft's live subject (parent pipes
   *     Compose's `onsubjectchange` into the entry so the label
   *     tracks user typing in real time, with "(no subject)" as
   *     the fallback);
   *   - a × close button on the right, hover-revealed to keep
   *     resting tabs visually quiet — matches the CLAUDE.md "row
   *     actions are opacity-0 by default" convention.
   *
   * Clicking the tab body restores that draft to a full-screen
   * modal; clicking × routes through the Compose's own `cancel`
   * function (handed up via `oncancelref` at mount) so Talk-room
   * delete + share-link cleanup + draft-source expunge all run,
   * the same as the modal-header × button does.
   *
   * Mounted only inside the mail view branch — the parent gates on
   * `currentView` so navigating to Calendar / Contacts / Settings
   * hides the bar without touching the underlying drafts.
   */

  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  interface BarItem {
    id: string
    subject: string
  }

  interface Props {
    items: BarItem[]
    onrestore: (id: string) => void
    onclose: (id: string) => void
  }

  let { items, onrestore, onclose }: Props = $props()
</script>

<!-- Outer wrapper anchors the strip to the bottom-right corner.
     `pointer-events-none` lets clicks fall through the gaps
     between tabs to the underlying mail UI; each tab re-enables
     events with `pointer-events-auto`.  `z-40` sits just under the
     Compose modal overlay (`z-50`) so an active modal still covers
     the bar instead of leaving phantom tabs poking out. -->
<div
  class="fixed bottom-0 right-4 z-40 max-w-[85vw] overflow-x-auto pointer-events-none"
  aria-label={m.compose_minimized_bar_aria_label()}
>
  <div class="flex items-end gap-1 pt-2 pb-0 px-1">
    {#each items as item (item.id)}
      <!-- `group` lets the × peer animate on hover; rounded-t-lg
           gives the tab its "rising from the bottom edge" shape
           without bottom rounding so the tab visually anchors to
           the viewport edge. -->
      <div
        class="group pointer-events-auto inline-flex items-center gap-2 px-3 py-2
               rounded-t-lg min-w-44 max-w-65 shadow-md
               bg-surface-100 dark:bg-surface-800
               border border-b-0 border-surface-300 dark:border-surface-700
               hover:bg-primary-500/10
               transition-colors"
      >
        <button
          type="button"
          class="flex-1 min-w-0 inline-flex items-center gap-2 text-left text-sm font-medium
                 text-surface-900 dark:text-surface-50 cursor-pointer"
          title={m.compose_minimized_item_title({
            subject: item.subject || m.compose_minimized_no_subject(),
          })}
          onclick={() => onrestore(item.id)}
        >
          <Icon name="compose" size={16} />
          <span class="truncate">
            {item.subject || m.compose_minimized_no_subject()}
          </span>
        </button>
        <button
          type="button"
          class="shrink-0 px-1 -mr-1 text-base leading-none cursor-pointer
                 text-surface-500 hover:text-surface-900 dark:hover:text-surface-100
                 opacity-0 group-hover:opacity-100 focus-visible:opacity-100
                 transition-opacity"
          aria-label={m.compose_minimized_item_close_aria_label()}
          title={m.compose_minimized_item_close_title()}
          onclick={(e) => {
            e.stopPropagation()
            onclose(item.id)
          }}
        >×</button>
      </div>
    {/each}
  </div>
</div>
