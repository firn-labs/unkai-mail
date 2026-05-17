<script lang="ts">
  /**
   * StandaloneEventEditor — entry component for a popped-out
   * EventEditor window (#304).  Takes a key from the URL, reads the
   * payload (calendars / draft / replyTo) stashed in localStorage by
   * the launcher, then mounts the regular `EventEditor` component
   * full-window.
   *
   * On a successful save the window emits
   * `event-editor-saved-from-popout` carrying the `SavedEvent` and
   * the (opaque) `replyTo` reference — the main window picks that
   * up and opens the final Compose pre-filled with the meeting
   * invite card as a reply to the original thread.  Cancel just
   * closes the window with no event; the main window has nothing
   * to do in that case.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { emit } from '@tauri-apps/api/event'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import EventEditor, { type SavedEvent } from './EventEditor.svelte'
  import {
    takeEventEditorPopoutPayload,
    type CalendarSummaryPopout,
    type EventEditorDraftPopout,
  } from './standaloneEventEditorWindow'
  import { applyTheme, installSystemModeListener, type ThemeMode } from './theme'

  let { popoutKey }: { popoutKey: string } = $props()

  // EventEditor expects real `Date` objects in its draft — we
  // rehydrate from the ISO strings stashed on the way in.
  interface RehydratedDraft {
    calendarId: string
    start: Date
    end: Date
    allDay?: boolean
    summary?: string
    description?: string
    location?: string
    url?: string
    attendees?: string[]
    requiredAttendees?: string[]
    optionalAttendees?: string[]
    chairAttendees?: string[]
    createTalkRoom?: boolean
  }

  let calendars = $state<CalendarSummaryPopout[]>([])
  let draft = $state<RehydratedDraft | null>(null)
  let replyTo = $state<unknown>(null)
  let loadError = $state('')

  $effect(() => {
    let unlistenSystem: (() => void) | null = null
    void (async () => {
      try {
        const prefs = await invoke<{
          theme_name: string
          theme_mode: ThemeMode
        }>('get_app_settings')
        applyTheme(prefs.theme_name, prefs.theme_mode)
        unlistenSystem = installSystemModeListener(
          prefs.theme_mode,
          prefs.theme_name,
        )
      } catch (e) {
        console.warn(
          'get_app_settings failed in standalone event editor',
          e,
        )
      }

      const stashed = takeEventEditorPopoutPayload(popoutKey)
      if (!stashed) {
        loadError = 'No popout state found for this event editor window.'
        return
      }
      calendars = stashed.calendars
      replyTo = stashed.replyTo ?? null
      draft = rehydrateDraft(stashed.draft)
    })()

    return () => {
      unlistenSystem?.()
    }
  })

  function rehydrateDraft(d: EventEditorDraftPopout): RehydratedDraft {
    return {
      ...d,
      start: new Date(d.start),
      end: new Date(d.end),
    }
  }

  function closeWindow() {
    void getCurrentWindow().close()
  }

  function onSaved(saved?: SavedEvent) {
    // Forward to the main window so it can open the final Compose
    // pre-filled with the meeting invite card.  We always emit
    // (even on undefined `saved`, which happens for the edit/delete
    // paths) so the main window can pick up state changes; but in
    // the #304 create-only flow `saved` is reliably present.
    void emit('event-editor-saved-from-popout', { saved, replyTo }).catch(
      (e) => {
        console.warn('event-editor-saved-from-popout emit failed', e)
      },
    )
    closeWindow()
  }
</script>

{#if loadError}
  <div
    class="h-screen w-screen flex items-center justify-center text-sm text-red-500 p-6 bg-surface-50 dark:bg-surface-900"
  >
    {loadError}
  </div>
{:else if draft}
  <!-- EventEditor's own `inStandaloneWindow` branch supplies the
       full-window surface, so we don't wrap it ourselves — wrapping
       would letterbox the form again (#304 follow-up). -->
  <EventEditor
    mode="create"
    calendars={calendars}
    draft={draft}
    inStandaloneWindow={true}
    onclose={closeWindow}
    onsaved={onSaved}
  />
{:else}
  <div
    class="h-screen w-screen flex items-center justify-center text-sm text-surface-500 bg-surface-50 dark:bg-surface-900"
  >
    Loading…
  </div>
{/if}
