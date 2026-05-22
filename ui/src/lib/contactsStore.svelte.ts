/**
 * contactsStore — single source of truth for the cached contact list.
 *
 * ContactsView used to hold the contact array as local component state,
 * which meant any other view that wanted to render a contact (e.g.
 * MailList wanting to show the sender's photo, #305) had to either
 * re-fetch the same data or wait for ContactsView to be mounted at
 * least once.  Lifting it into a `.svelte.ts` module makes the list
 * reactive across the whole app and pays for itself the second time
 * any view needs to render contact information.
 *
 * The store does not own *sync* — Nextcloud sync is still driven by
 * ContactsView's UI (it's a user-facing "refresh" gesture).  This
 * module only owns the local-cache read and the lookup index.
 */

import { invoke, convertFileSrc } from '@tauri-apps/api/core'

export interface ContactAddress {
  kind: string
  street: string
  locality: string
  region: string
  postal_code: string
  country: string
}
export interface ContactPhone {
  kind: string
  value: string
}
export interface ContactEmail {
  kind: string
  value: string
}
export interface StructuredName {
  family: string
  given: string
  additional: string
  prefix: string
  suffix: string
}
export interface ContactImpp {
  kind: string
  value: string
}

/** Mirrors the Rust `unkai_core::models::Contact` shape that
 *  `get_contacts` returns.  Fields stay aligned with ContactsView's
 *  in-component interface so that view can swap straight onto the
 *  store without a translation layer. */
export interface Contact {
  id: string
  nextcloud_account_id: string
  display_name: string
  email: ContactEmail[]
  phone: ContactPhone[]
  organization: string | null
  photo_mime: string | null
  photo_data: number[] | null
  title?: string | null
  birthday?: string | null
  note?: string | null
  addresses?: ContactAddress[]
  urls?: string[]
  categories?: string[]
  addressbook?: string
  structured_name?: StructuredName
  nickname?: string | null
  anniversary?: string | null
  gender?: string | null
  impp?: ContactImpp[]
  role?: string | null
  languages?: string[]
  geo?: string | null
  timezone?: string | null
  keys?: string[]
}

class ContactsStore {
  /** Sorted-by-display-name list, mirroring how ContactsView used to
   *  hold it locally.  Sort lives here so every consumer gets the
   *  same order without re-sorting per-view. */
  list = $state<Contact[]>([])

  /** Map keyed by lowercase email so callers (MailList rows,
   *  mailing-list member rows in ContactsView) can resolve a sender's
   *  card from just the email string parsed off the From header. */
  byEmail = $derived.by(() => {
    const m = new Map<string, Contact>()
    for (const c of this.list) {
      for (const e of c.email) {
        if (e.value) m.set(e.value.toLowerCase(), c)
      }
    }
    return m
  })

  async load(): Promise<void> {
    try {
      const fetched = await invoke<Contact[]>('get_contacts', { ncId: null })
      fetched.sort((a, b) =>
        a.display_name.localeCompare(b.display_name, undefined, { sensitivity: 'base' }),
      )
      this.list = fetched
    } catch (e) {
      // Cache may be FIDO-locked at boot, or `get_contacts` may be a
      // no-op if the user hasn't connected a Nextcloud account yet.
      // Either way, leaving the list empty just means MailList falls
      // back to From-header initials — a graceful degradation rather
      // than a hard failure.
      console.warn('contactsStore.load failed', e)
    }
  }
}

export const contactsStore = new ContactsStore()

/** Build a `<img src>`-ready URL against Tauri's custom URI scheme.
 *  Bytes stream straight from the cache to the webview — no JSON
 *  bloat, browser handles caching, callers pair this with
 *  `loading="lazy"` to defer off-screen rows. */
export function contactPhotoSrc(c: Contact | undefined | null): string | null {
  if (!c?.photo_mime) return null
  return convertFileSrc(c.id, 'contact-photo')
}
