/**
 * The UI's one door to the backend (#473).
 *
 * Usage in components:
 *
 *   import * as api from './api'            // from ui/src/lib/*
 *   import * as api from './lib/api'        // from ui/src/App.svelte
 *
 *   const envelopes = await api.mail.fetchEnvelopes({ accountId, folder, limit })
 *   const un = await api.onAppEvent('new-mail', (e) => { … })
 *
 * Domain namespaces mirror the backend's areas of responsibility; the
 * event helpers and their name registry live in `./events`; desktop
 * platform affordances (dialogs, notifications, autostart, asset URLs)
 * in `./platform`.
 */

export * as accounts from './accounts'
export * as mail from './mail'
export * as compose from './compose'
export * as contacts from './contacts'
export * as calendar from './calendar'
export * as nextcloud from './nextcloud'
export * as talk from './talk'
export * as notes from './notes'
export * as profiles from './profiles'
export * as tasks from './tasks'
export * as crypto from './crypto'
export * as settings from './settings'
export * as system from './system'
export * as platform from './platform'

export {
  onAppEvent,
  emitAppEvent,
  emitAppEventToParent,
  SIGNATURE_UPDATED_EVENT,
  SIGNATURE_POPOUT_CLOSED_EVENT,
  type AppEventName,
  type AppEventPayloads,
} from './events'

export type * from './types'
