<script lang="ts">
  /**
   * ContactsView — list / edit / add / delete Nextcloud contacts.
   *
   * Reads from the local cache (`get_contacts`) so the list paints
   * instantly, then kicks off a fresh `sync_nextcloud_contacts` in the
   * background. Any edit the user makes writes through to both the
   * server (PUT / DELETE) and the cache, so we don't have to wait for
   * the next sync to see our own changes.
   */

  import * as api from './api'
  import type { ImportContactsReport } from './api/types'
  import { anchorRect, cursorAnchor } from './coords'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'
  import Avatar from './Avatar.svelte'
  import EmojiPicker from './EmojiPicker.svelte'
  import Icon, { type IconName } from './Icon.svelte'
  import SearchInput from './SearchInput.svelte'
  import Select from './Select.svelte'
  import DateField from './DateField.svelte'
  import AddressSuggestField from './AddressSuggestField.svelte'
  import { contactsStore, contactPhotoSrc } from './contactsStore.svelte'
  import { resizableSidebar } from './resizableSidebar'

  // ── Types (mirror the Rust models) ──────────────────────────
  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }
  interface ContactAddress {
    kind: string
    street: string
    locality: string
    region: string
    postal_code: string
    country: string
  }
  interface ContactPhone {
    /** "home" / "work" / "cell" / "fax" / "other" — pulled from the
        vCard `TEL;TYPE=…` parameter. */
    kind: string
    value: string
  }
  interface ContactEmail {
    /** "home" / "work" / "other" — pulled from the vCard
        `EMAIL;TYPE=…` parameter. */
    kind: string
    value: string
  }
  /** Mirrors `unkai_core::models::StructuredName`. */
  interface StructuredName {
    family: string
    given: string
    additional: string
    prefix: string
    suffix: string
  }
  /** Mirrors `unkai_core::models::ContactImpp`. */
  interface ContactImpp143 {
    kind: string
    value: string
  }
  interface Contact {
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
    /** vCard CATEGORIES — the Contact Groups the contact
     *  belongs to.  Mutated by drag-drop onto a Contact Group
     *  row in the sidebar; sync goes back to NC via
     *  `add_contact_to_category`. */
    categories?: string[]
    /** CardDAV addressbook path the contact lives in. */
    addressbook?: string
    // ── #143 vCard 4 fields ────────────────────────────────
    structured_name?: StructuredName
    nickname?: string | null
    anniversary?: string | null
    gender?: string | null
    impp?: ContactImpp143[]
    role?: string | null
    languages?: string[]
    geo?: string | null
    timezone?: string | null
    /** PGP / X.509 keys (round-tripped today; no form UI yet). */
    keys?: string[]
  }
  interface ContactInput {
    display_name: string
    emails: ContactEmail[]
    phones: ContactPhone[]
    organization: string | null
    photo_mime: string | null
    photo_data: number[] | null
    /** Optional extended fields. The Rust side merges them over the
        cached vCard so omitting a field preserves whatever was on
        the server, instead of clearing it. */
    title?: string | null
    birthday?: string | null
    note?: string | null
    addresses?: ContactAddress[]
    urls?: string[]
    // ── #143 vCard 4 fields ────────────────────────────────
    structured_name?: StructuredName
    nickname?: string | null
    anniversary?: string | null
    gender?: string | null
    impp?: ContactImpp143[]
    role?: string | null
    languages?: string[]
    geo?: string | null
    timezone?: string | null
    categories?: string[]
    /** #57 — armored OpenPGP `KEY:` values to round-trip through
     *  the vCard.  Optional so callers that don't include the
     *  field leave existing keys intact via the backend's
     *  merge-over-cached-vCard semantics. */
    keys?: string[]
  }
  interface AddressbookSummary {
    path: string
    name: string
    display_name: string | null
  }

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<NextcloudAccount[]>([])
  // Contact list lives on the shared `contactsStore` (`./contactsStore.svelte`)
  // so MailList rows (#305) can resolve sender → contact without
  // having to mount ContactsView first.  Reads still use the same
  // `contactsStore.list` shape that this view's local state used to
  // expose.
  const contacts = $derived(contactsStore.list)
  let loading = $state(true)
  let syncing = $state(false)
  let error = $state('')

  // Currently selected contact in the list. `null` = nothing selected.
  // `'new'` is a sentinel meaning "showing the create form" — easier
  // than a separate boolean since the editor pane is one component.
  let selectedId = $state<string | 'new' | null>(null)

  // Form state — bound to inputs. When `selectedId` changes we
  // copy the matching contact's fields into these so edits don't
  // mutate the cached row until the user saves.
  let formName = $state('')
  // ── #143: extended vCard 4 fields ────────────────────────────
  // `display_name` (formerly the only name field) is auto-derived
  // from the structured-name parts at save time when the user
  // fills those in but leaves `formName` blank.  When both are
  // filled, the explicit `formName` wins so users can override
  // the derivation (e.g. "Jane S." instead of "Jane Smith").
  let formFamily = $state('')
  let formGiven = $state('')
  let formAdditional = $state('')
  let formPrefix = $state('')
  let formSuffix = $state('')
  let formNickname = $state('')
  let formAnniversary = $state('')
  let formGender = $state('')
  let formRole = $state('')
  let formTimezone = $state('')
  /** IMPP rows — same shape as phone / email but with a wider
   *  kind set (matrix / xmpp / telegram / signal / skype / whatsapp
   *  / other) so the user can group platform-by-platform. */
  interface ContactImpp {
    kind: string
    value: string
  }
  let formImpp = $state<ContactImpp[]>([])
  /** Languages typed as a chip list, comma- or newline-separated
   *  on input.  Kept as a Vec on the wire (RFC 6350 LANG is one
   *  property per language). */
  let formLanguages = $state<string[]>([])
  let formLanguageDraft = $state('')
  /** Categories chip list — same UX as languages. */
  let formCategories = $state<string[]>([])
  let formCategoryDraft = $state('')
  /** MIME of a freshly-uploaded photo.  Pairs with
   *  `selectedPhotoBytes`; we keep them as separate cells so the
   *  upload pipeline can update them in lockstep without having
   *  to round-trip through the cached contact. */
  let formPhotoMime = $state<string | null>(null)
  // Same per-row treatment as phones — each email carries a kind
  // picker (Home / Work / Other) so the vCard `EMAIL;TYPE=…`
  // round-trips and Nextcloud Contacts groups identically.
  let formEmails = $state<ContactEmail[]>([])
  // Phones are now per-row records so each carries a kind picker
  // ("home" / "work" / "mobile" / "fax" / "other"), matching what
  // Nextcloud Contacts shows.
  let formPhones = $state<ContactPhone[]>([])
  let formOrg = $state('')
  let formTitle = $state('')
  let formBirthday = $state('')
  let formNote = $state('')
  /** OpenPGP keys (#57).  One row per `KEY:` value on the
   *  vCard — armored ASCII pasted by the user.  Same shape as
   *  `formWebsites` so the add / remove / save patterns can be
   *  reused.  Wrapped in an object because Svelte's `bind:value`
   *  on a primitive `string` in an array can't write back through
   *  `bind:` without a wrapping shape. */
  let formKeys = $state<{ value: string }[]>([])

  /** Websites — one row per URL, mirroring the phone / email
   *  per-row pattern so the user can add and remove entries
   *  without juggling a multi-line textarea. */
  let formWebsites = $state<{ value: string }[]>([])
  // Addresses are an array of records, edited in place. We model a
  // single concatenated free-text field per address keeping
  // street/locality/region/postal/country on separate lines so the
  // form stays readable without exploding into one input per slot.
  let formAddresses = $state<ContactAddress[]>([])
  let formAccountId = $state('')       // only used for create
  let formAddressbookUrl = $state('')  // only used for create
  let formAddressbookName = $state('') // only used for create

  /** Tracks the privacy toggle for online address suggestions
   *  (#259, sharing the gate with #280's location autocomplete).
   *  Off by default; flipping it on in General Settings opts the
   *  user into Nominatim queries.  `AddressSuggestField` honours
   *  this flag so a contact form opened with the toggle off
   *  stays a plain set of inputs that never touch the network. */
  let geocodingEnabled = $state(false)
  $effect(() => {
    void api.settings.getAppSettings()
      .then((s) => {
        geocodingEnabled = s.location_geocoding_enabled === true
      })
      .catch(() => {
        geocodingEnabled = false
      })
  })
  // Photo bytes for the selected contact, lazy-loaded via
  // `get_contact_photo`. Only fetched so we can round-trip them
  // through `update_contact` — display uses the `contact-photo://`
  // URI scheme, which streams bytes straight to `<img>` without
  // touching JSON IPC. Without this round-trip the vCard rebuild
  // on save would drop the avatar.
  let selectedPhotoBytes = $state<number[] | null>(null)
  /** True after the user clicks "Remove photo" — needed because a
   *  null photo payload means "no change" to the backend merge (so
   *  a save racing the lazy byte load can't wipe the avatar), so
   *  removal has to be signalled explicitly: buildInput sends
   *  `photo_mime: ''` (the empty-string-clears contract, #522). */
  let photoRemoved = $state(false)
  let saving = $state(false)
  let formError = $state('')
  let deleteConfirm = $state(false)
  /** #143 follow-up: tri-state mode for the right pane.
   *
   *   - `selectedId === null`              → empty state
   *   - `selectedId === 'new'`             → creation form
   *   - `selectedId` set + `editing=false` → read-only view
   *   - `selectedId` set + `editing=true`  → edit form
   *
   *  The flag is reset to `false` whenever the user picks a
   *  different contact so navigation always lands in the
   *  read-only view first; the Edit button toggles it back on.
   */
  let editing = $state(false)

  // Cache per-account addressbooks so switching the "save to" account
  // in the new-contact form doesn't re-hit the server.
  let addressbooksByAccount = $state<Record<string, AddressbookSummary[]>>({})

  // ── Import from file (#484) ──────────────────────────────────
  // Modal state: target account + addressbook + the picked file's
  // absolute path (the dialog hands us a path; the Rust command does
  // the file IO, same split as the theme / settings-bundle imports).
  let importForm = $state<{
    accountId: string
    addressbookUrl: string
    addressbookName: string
    path: string
  } | null>(null)
  let importBusy = $state(false)
  let importError = $state('')
  let importReport = $state<ImportContactsReport | null>(null)

  // ── Categories + mailing lists (#133 redesign) ───────────────
  interface ContactCategoryView {
    name: string
    memberCount: number
    useAsMailingList: boolean
  }
  interface MailingListView {
    id: string
    source: 'category' | 'team' | 'manual'
    name: string
    members: { displayName: string; email: string }[]
    hiddenFromAutocomplete: boolean
    emoji: string | null
  }
  type ContactsTab = 'contacts' | 'lists'
  let activeTab = $state<ContactsTab>('contacts')
  /** Distinct addressbook paths across the cached contacts —
   *  populated lazily once `accounts` resolves so the sidebar
   *  can render one row per CardDAV collection.  Each row is
   *  `{ ncId, path, name, displayName? }`. */
  let allAddressbooks = $state<
    { ncId: string; path: string; name: string; displayName: string | null }[]
  >([])
  let categories = $state<ContactCategoryView[]>([])
  let mailingLists = $state<MailingListView[]>([])
  /** Currently-active sidebar selection on the Contacts tab.
   *  Strings: `'all'` | `'addressbook:<path>'` | `'category:<name>'`. */
  let selectedScope = $state<string>('all')

  /** Drag state for the drop-a-contact-on-a-Contact Group
   *  flow.  Carries the *app-side* contact id (`nc::uid`)
   *  since the IPC takes the composite id, not the bare UID. */
  let draggedContactId = $state<string | null>(null)
  let dragHoverCategory = $state<string | null>(null)
  /** Named tab-switch handler — extracted because inline arrow
   *  functions inside `onclick={(e) => { … }}` were not
   *  consistently swapping `activeTab`; routing through a
   *  named function fixes the binding and gives the menu
   *  cleanup a single place to live. */
  function selectTab(t: ContactsTab) {
    openMenuFor = null
    activeTab = t
  }
  /** Selected mailing list on the Lists tab — the middle
   *  column shows its members. */
  let selectedListId = $state<string | null>(null)
  /** Member-search filter shown above the member list in the
   *  middle column when a list is selected. */
  let memberQuery = $state('')
  /** Open / closed state for the inline "+ Add Contact"
   *  picker — when on, the middle column flips from "show
   *  members" to "pick contacts to add". */
  let pickerOpen = $state(false)
  let pickerQuery = $state('')
  /** Which row's three-dot menu is open (Lists tab + Contacts
   *  tab Contact Groups).  String id keys keep the lookup
   *  cheap and let one popover replace another by simply
   *  reassigning. */
  let openMenuFor = $state<string | null>(null)
  /** Page-relative anchor for the open three-dot menu — set
   *  by the trigger's onclick.  `position: fixed` lets the
   *  menu escape the sidebar's `overflow-y-auto` clip; the
   *  alternative (absolute inside an overflow-clipped parent)
   *  cuts off menus that pop past the sidebar's edge. */
  let menuTop = $state(0)
  let menuLeft = $state(0)
  /** Inline rename state for a mailing-list row.  When set, the
   *  matching row's name span renders an `<input>` instead. */
  let renamingListId = $state<string | null>(null)
  let renameValue = $state('')
  /** Anchor + open state for the per-row emoji picker popover. */
  let emojiPickerFor = $state<string | null>(null)
  let emojiPickerTop = $state(0)
  let emojiPickerLeft = $state(0)
  $effect(() => {
    if (!emojiPickerFor) return
    const onDoc = () => (emojiPickerFor = null)
    const handle = setTimeout(() => document.addEventListener('mousedown', onDoc), 0)
    return () => {
      clearTimeout(handle)
      document.removeEventListener('mousedown', onDoc)
    }
  })
  // Close any open three-dot menu when the user clicks
  // anywhere outside one — same idiom we use elsewhere for
  // popover dismissal.  The menu's own `onclick` calls
  // `e.stopPropagation()` so item picks don't immediately
  // close before their handler fires.
  $effect(() => {
    if (!openMenuFor) return
    const onDoc = () => (openMenuFor = null)
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  })
  /** Mailing-list rows partitioned by source so each section
   *  can render its own header.  No search filter here — the
   *  Lists tab's search box was moved to the right pane (it
   *  filters members of the selected list, not lists
   *  themselves), so this derived just splits by source. */
  const filteredMailingLists = $derived({
    manual: mailingLists.filter((m) => m.source === 'manual'),
    category: mailingLists.filter((m) => m.source === 'category'),
    team: mailingLists.filter((m) => m.source === 'team'),
  })
  const selectedList = $derived(
    selectedListId ? mailingLists.find((m) => m.id === selectedListId) ?? null : null,
  )

  async function loadSidebarData() {
    // Addressbooks: list per NC account, dedupe by composite
    // (ncId, path).  We don't show empty addressbooks the user
    // hasn't synced yet — the contacts list is the source of
    // truth.
    try {
      const seen = new Set<string>()
      const rows: typeof allAddressbooks = []
      for (const a of accounts) {
        try {
          const books = await api.contacts.listNextcloudAddressbooks({ ncId: a.id })
          for (const b of books) {
            const k = `${a.id}::${b.path}`
            if (seen.has(k)) continue
            seen.add(k)
            rows.push({
              ncId: a.id,
              path: b.path,
              name: b.name,
              displayName: b.display_name,
            })
          }
        } catch (e) {
          console.warn('list_nextcloud_addressbooks failed for', a.id, e)
        }
      }
      allAddressbooks = rows
    } catch (e) {
      console.warn('addressbooks load failed', e)
    }
    try {
      categories = await api.contacts.listContactCategories()
    } catch (e) {
      console.warn('list_contact_categories failed', e)
    }
    try {
      mailingLists = await api.contacts.listMailingLists()
    } catch (e) {
      console.warn('list_mailing_lists failed', e)
    }
  }

  // ── Contact Group (CATEGORIES) CRUD ────────────────────────
  async function createCategory() {
    const name = prompt('New Contact Group — name?')?.trim()
    if (!name) return
    if (contacts.length === 0) {
      formError = 'Add at least one contact before creating a Contact Group — a tag with no contacts vanishes on the next sync.'
      return
    }
    const seedRaw = prompt(
      `Seed members — paste contact emails separated by commas (or leave blank to add later via drag-drop).`,
    )
    if (seedRaw === null) return
    const seedEmails = new Set(
      seedRaw
        .split(',')
        .map((s) => s.trim().toLowerCase())
        .filter(Boolean),
    )
    const seedIds = contacts
      .filter((c) => c.email.some((e) => seedEmails.has(e.value.toLowerCase())))
      .map((c) => c.id)
    if (seedIds.length === 0) {
      // No matching contacts — the category would vanish.
      // Bail with a hint rather than silently doing nothing.
      formError = 'None of the pasted emails matched a cached contact — Contact Group not created.'
      return
    }
    for (const id of seedIds) {
      try {
        await api.contacts.addContactToCategory({ contactId: id, category: name })
      } catch (e) {
        console.warn('seed category member failed', id, e)
      }
    }
    await reloadContacts()
    selectedScope = `category:${name}`
  }
  async function renameCategory(name: string) {
    const next = prompt('Rename Contact Group', name)?.trim()
    if (!next || next === name) return
    try {
      await api.contacts.renameContactCategory({ old: name, new: next })
      await reloadContacts()
      if (selectedScope === `category:${name}`) selectedScope = `category:${next}`
    } catch (e) {
      formError = formatError(e) || 'Failed to rename Contact Group'
    }
  }
  async function deleteCategory(name: string) {
    if (!confirm(`Remove the "${name}" tag from every contact carrying it? Contacts themselves are kept.`)) return
    try {
      await api.contacts.deleteContactCategory({ name })
      await reloadContacts()
      if (selectedScope === `category:${name}`) selectedScope = 'all'
    } catch (e) {
      formError = formatError(e) || 'Failed to delete Contact Group'
    }
  }
  async function toggleCategoryAsList(name: string, currentlyOn: boolean) {
    try {
      await api.contacts.setCategoryUseAsMailingList({
        name,
        enabled: !currentlyOn,
      })
      categories = categories.map((c) =>
        c.name === name ? { ...c, useAsMailingList: !currentlyOn } : c,
      )
      // Optimistic local update — refetching list_mailing_lists
      // would round-trip OCS + Circles and stall the swatch.
      mailingLists = mailingLists.map((m) =>
        m.id === `cat:${name}` ? { ...m, hiddenFromAutocomplete: currentlyOn } : m,
      )
    } catch (e) {
      formError = formatError(e) || 'Failed to toggle "Use as mailing list"'
    }
  }
  async function addContactIdToCategory(contactId: string, name: string) {
    try {
      await api.contacts.addContactToCategory({ contactId, category: name })
      await reloadContacts()
    } catch (e) {
      formError = formatError(e) || 'Failed to tag contact'
    }
  }

  // ── Manual mailing list CRUD ──────────────────────────────
  let newListForm = $state<{ name: string; emoji: string | null } | null>(null)
  let newListBusy = $state(false)
  let newListError = $state('')

  function openNewMailingListForm() {
    newListForm = { name: '', emoji: null }
    newListError = ''
  }

  async function commitNewMailingList() {
    if (!newListForm) return
    const name = newListForm.name.trim()
    if (!name) return
    if (accounts.length === 0) {
      newListError = 'No Nextcloud account is connected.'
      return
    }
    newListBusy = true
    newListError = ''
    const ncId = accounts[0].id
    let books = addressbooksByAccount[ncId]
    if (!books) {
      try {
        books = await api.contacts.listNextcloudAddressbooks({ ncId })
        addressbooksByAccount[ncId] = books
      } catch (e) {
        newListError = formatError(e) || 'Failed to list addressbooks'
        newListBusy = false
        return
      }
    }
    const book = books[0]
    if (!book) {
      newListError = 'No addressbook available.'
      newListBusy = false
      return
    }
    const chosenEmoji = newListForm.emoji
    try {
      const created = await api.contacts.createContactGroup({
        ncId,
        addressbookUrl: book.path,
        addressbookName: book.name,
        displayName: name,
        memberUids: [],
      })
      // Persist the chosen emoji against the unified
      // mailing-list id (`list:<vcardUid>`).  Best-effort:
      // failures here only mean the row falls back to its
      // source icon, not that the create itself failed.
      if (chosenEmoji && created?.id) {
        try {
          await api.contacts.setMailingListEmoji({
            id: `list:${created.id}`,
            emoji: chosenEmoji,
          })
        } catch (e) {
          console.warn('set_mailing_list_emoji failed', e)
        }
      }
      try {
        mailingLists = await api.contacts.listMailingLists()
      } catch (e) {
        console.warn('list_mailing_lists refresh failed', e)
      }
      newListForm = null
    } catch (e) {
      newListError = formatError(e) || 'Failed to create mailing list'
    } finally {
      newListBusy = false
    }
  }
  async function deleteManualMailingList(id: string, name: string) {
    if (!confirm(`Delete mailing list "${name}"? Members are not affected.`)) return
    // Manual rows use `list:<vcard-uid>` ids; the underlying
    // group_id (composite contact id) is `nc::uid`.  Strip the
    // prefix to get back to the contact-handle id.
    const groupId = id.startsWith('list:') ? id.slice(5) : id
    try {
      await api.contacts.deleteContactGroup({ groupId })
      mailingLists = mailingLists.filter((m) => m.id !== id)
    } catch (e) {
      formError = formatError(e) || 'Failed to delete mailing list'
    }
  }
  /** Add a contact to the currently-selected mailing list.
   *  Manual lists go through `update_contact_group`; category
   *  lists go through `add_contact_to_category`.  Teams are
   *  read-only — the caller never reaches this for a team.
   *  Updates the UI optimistically so the row appears instantly;
   *  the CardDAV PUT happens in the background. */
  async function addContactToSelectedList(contactId: string) {
    if (!selectedList) return
    const ml = selectedList
    const target = contacts.find((c) => c.id === contactId)
    if (!target) return
    const memberView = {
      displayName: target.display_name,
      email: target.email[0]?.value ?? '',
    }
    if (ml.source === 'manual') {
      const groupId = ml.id.startsWith('list:') ? ml.id.slice(5) : ml.id
      const currentUids = ml.members
        .map((m) => {
          const c = contacts.find((cc) =>
            cc.email.some((e) => e.value.toLowerCase() === m.email.toLowerCase()),
          )
          return c ? bareUidOfContact(c) : null
        })
        .filter((u): u is string => !!u)
      const targetUid = bareUidOfContact(target)
      if (currentUids.includes(targetUid)) return
      mailingLists = mailingLists.map((m) =>
        m.id === ml.id ? { ...m, members: [...m.members, memberView] } : m,
      )
      try {
        await api.contacts.updateContactGroup({
          groupId,
          displayName: null,
          memberUids: [...currentUids, targetUid],
        })
      } catch (e) {
        // Roll back the optimistic add on failure.
        mailingLists = mailingLists.map((m) =>
          m.id === ml.id ? { ...m, members: m.members.filter((mm) => mm !== memberView) } : m,
        )
        formError = formatError(e) || 'Failed to add member'
      }
    } else if (ml.source === 'category') {
      mailingLists = mailingLists.map((m) =>
        m.id === ml.id && memberView.email
          ? { ...m, members: [...m.members, memberView] }
          : m,
      )
      contactsStore.list = contacts.map((c) => {
        if (c.id !== contactId) return c
        const cats = c.categories ?? []
        return cats.includes(ml.name) ? c : { ...c, categories: [...cats, ml.name] }
      })
      try {
        await api.contacts.addContactToCategory({ contactId, category: ml.name })
      } catch (e) {
        await reloadContacts()
        formError = formatError(e) || 'Failed to tag contact'
      }
    }
  }

  /** Remove a member (by email) from the currently-selected
   *  mailing list.  Same source split as `addContactToSelectedList`. */
  async function removeContactFromSelectedList(email: string) {
    if (!selectedList) return
    const ml = selectedList
    const lower = email.toLowerCase()
    if (ml.source === 'manual') {
      const groupId = ml.id.startsWith('list:') ? ml.id.slice(5) : ml.id
      const remainingUids = ml.members
        .filter((m) => m.email.toLowerCase() !== lower)
        .map((m) => {
          const c = contacts.find((cc) =>
            cc.email.some((e) => e.value.toLowerCase() === m.email.toLowerCase()),
          )
          return c ? bareUidOfContact(c) : null
        })
        .filter((u): u is string => !!u)
      const before = ml.members
      mailingLists = mailingLists.map((m) =>
        m.id === ml.id
          ? { ...m, members: m.members.filter((mm) => mm.email.toLowerCase() !== lower) }
          : m,
      )
      try {
        await api.contacts.updateContactGroup({
          groupId,
          displayName: null,
          memberUids: remainingUids,
        })
      } catch (e) {
        mailingLists = mailingLists.map((m) =>
          m.id === ml.id ? { ...m, members: before } : m,
        )
        formError = formatError(e) || 'Failed to remove member'
      }
    } else if (ml.source === 'category') {
      const target = contacts.find((c) =>
        c.email.some((e) => e.value.toLowerCase() === lower),
      )
      if (!target) return
      mailingLists = mailingLists.map((m) =>
        m.id === ml.id
          ? { ...m, members: m.members.filter((mm) => mm.email.toLowerCase() !== lower) }
          : m,
      )
      contactsStore.list = contacts.map((c) =>
        c.id === target.id
          ? { ...c, categories: (c.categories ?? []).filter((cat) => cat !== ml.name) }
          : c,
      )
      try {
        await api.contacts.removeContactFromCategory({
          contactId: target.id,
          category: ml.name,
        })
      } catch (e) {
        await reloadContacts()
        formError = formatError(e) || 'Failed to untag contact'
      }
    }
  }
  /** Bare vcard UID — composite ids look like `nc::uid`. */
  function bareUidOfContact(c: Contact): string {
    const segs = c.id.split('::')
    return segs[1] ?? c.id
  }

  async function toggleMailingListHidden(id: string, currently: boolean) {
    try {
      await api.contacts.setMailingListHidden({ id, hidden: !currently })
      mailingLists = mailingLists.map((m) =>
        m.id === id ? { ...m, hiddenFromAutocomplete: !currently } : m,
      )
    } catch (e) {
      formError = formatError(e) || 'Failed to toggle hide flag'
    }
  }

  function startRenameMailingList(ml: MailingListView) {
    renamingListId = ml.id
    renameValue = ml.name
  }

  async function commitRenameMailingList(ml: MailingListView) {
    const next = renameValue.trim()
    renamingListId = null
    if (!next || next === ml.name) return
    try {
      await api.contacts.renameMailingList({ id: ml.id, newName: next })
      // Category renames change the row's id (cat:<old> →
      // cat:<new>); refetch picks up the new id while keeping
      // per-row settings in sync.  Manual lists keep their id.
      if (ml.source === 'category') {
        if (selectedListId === ml.id) selectedListId = `cat:${next}`
        mailingLists = await api.contacts.listMailingLists()
        await reloadContacts()
      } else {
        mailingLists = mailingLists.map((m) =>
          m.id === ml.id ? { ...m, name: next } : m,
        )
      }
    } catch (e) {
      formError = formatError(e) || 'Failed to rename mailing list'
    }
  }

  /** Swap the open three-dot menu for the emoji picker at the same
   *  fixed anchor — the menu is where the user's attention already
   *  is, and its coordinates are the only ones still valid once the
   *  menu unmounts. */
  function openEmojiPickerAtMenu(ml: MailingListView) {
    emojiPickerTop = menuTop
    emojiPickerLeft = menuLeft
    emojiPickerFor = ml.id
    openMenuFor = null
  }

  async function pickMailingListEmoji(ml: MailingListView, emoji: string | null) {
    emojiPickerFor = null
    try {
      await api.contacts.setMailingListEmoji({ id: ml.id, emoji })
      mailingLists = mailingLists.map((m) =>
        m.id === ml.id ? { ...m, emoji } : m,
      )
    } catch (e) {
      formError = formatError(e) || 'Failed to set emoji'
    }
  }

  // Naive free-text filter over the loaded list. Server-side search
  // isn't needed at this scale (addressbooks are usually hundreds,
  // not thousands, of contacts).
  let query = $state('')
  const filteredContacts = $derived.by(() => {
    const q = query.trim().toLowerCase()
    let scope = contacts
    if (selectedScope.startsWith('addressbook:')) {
      const name = selectedScope.slice('addressbook:'.length)
      // The cache stores the addressbook *name* (the last
      // segment of the CardDAV URL — e.g. "contacts") as each
      // contact's `addressbook` value, so filtering compares
      // names, not full URLs.
      scope = contacts.filter((c) => c.addressbook === name)
    } else if (selectedScope.startsWith('category:')) {
      const name = selectedScope.slice('category:'.length)
      scope = contacts.filter((c) => c.categories?.includes(name))
    }
    if (!q) return scope
    return scope.filter(
      (c) =>
        c.display_name.toLowerCase().includes(q) ||
        c.email.some((e) => e.value.toLowerCase().includes(q)) ||
        (c.organization ?? '').toLowerCase().includes(q),
    )
  })

  $effect(() => {
    void init()
  })


  async function init() {
    loading = true
    error = ''
    try {
      accounts = await api.nextcloud.getNextcloudAccounts()
      if (accounts.length === 0) {
        error = 'Connect a Nextcloud account first to sync contacts.'
        loading = false
        return
      }
      await reloadContacts()
      // Default the create-form account selector to the first NC so
      // the user doesn't have to pick when they only have one.
      formAccountId = accounts[0].id
      void loadAddressbooksFor(formAccountId)
    } catch (e) {
      error = formatError(e) || 'Failed to load contacts'
    } finally {
      loading = false
    }
    // Kick off a refresh in the background so new/changed contacts
    // from other devices land without the user having to visit the
    // Nextcloud settings page.
    void syncInBackground()
  }

  async function reloadContacts() {
    await contactsStore.load()
    await loadSidebarData()
  }

  async function syncInBackground() {
    if (syncing) return
    syncing = true
    try {
      for (const a of accounts) {
        try {
          await api.contacts.syncNextcloudContacts({ ncId: a.id })
        } catch (e) {
          console.warn('sync_nextcloud_contacts failed for', a.id, e)
        }
      }
      await reloadContacts()
    } finally {
      syncing = false
    }
  }

  async function loadAddressbooksFor(ncId: string) {
    if (addressbooksByAccount[ncId]) {
      applyAddressbookDefault(ncId)
      return
    }
    try {
      const books = await api.contacts.listNextcloudAddressbooks({ ncId })
      addressbooksByAccount[ncId] = books
      applyAddressbookDefault(ncId)
    } catch (e) {
      console.warn('list_nextcloud_addressbooks failed', e)
    }
  }

  function applyAddressbookDefault(ncId: string) {
    const books = addressbooksByAccount[ncId] ?? []
    if (books.length > 0) {
      formAddressbookUrl = books[0].path
      formAddressbookName = books[0].name
    } else {
      formAddressbookUrl = ''
      formAddressbookName = ''
    }
  }

  // ── Import from file (#484) ──────────────────────────────────

  function openImport() {
    importError = ''
    importReport = null
    const accountId = accounts[0]?.id ?? ''
    importForm = { accountId, addressbookUrl: '', addressbookName: '', path: '' }
    if (accountId) void setImportAccount(accountId)
  }

  /** Load (or reuse) the account's addressbooks and default the
      target to the first one. Deliberately doesn't share
      `applyAddressbookDefault` — that helper writes the *create
      form's* fields, and clobbering them from the import modal
      would surprise the next "New contact". */
  async function setImportAccount(ncId: string) {
    if (!importForm) return
    importForm.accountId = ncId
    let books = addressbooksByAccount[ncId]
    if (!books) {
      try {
        books = await api.contacts.listNextcloudAddressbooks({ ncId })
        addressbooksByAccount[ncId] = books
      } catch (e) {
        importError = formatError(e)
        books = []
      }
    }
    // The user may have switched accounts while the listing was in
    // flight — don't apply a stale default.
    if (!importForm || importForm.accountId !== ncId) return
    importForm.addressbookUrl = books[0]?.path ?? ''
    importForm.addressbookName = books[0]?.name ?? ''
  }

  function onImportAddressbookChange(path: string) {
    if (!importForm) return
    const books = addressbooksByAccount[importForm.accountId] ?? []
    importForm.addressbookUrl = path
    importForm.addressbookName = books.find((b) => b.path === path)?.name ?? ''
  }

  async function pickImportFile() {
    const picked = await api.platform.openFileDialog({
      multiple: false,
      filters: [{ name: 'Contacts', extensions: ['vcf', 'vcard', 'csv'] }],
    })
    if (typeof picked === 'string' && importForm) {
      importForm.path = picked
      importReport = null
      importError = ''
    }
  }

  /** Display name for the picked file — the dialog returns an
      absolute path; the basename is all the modal needs to show. */
  function importFileName(path: string): string {
    return path.split(/[\\/]/).pop() ?? path
  }

  async function runImport() {
    if (!importForm || !importForm.path || !importForm.addressbookUrl || importBusy) return
    importBusy = true
    importError = ''
    try {
      importReport = await api.contacts.importContactsFile({
        ncId: importForm.accountId,
        addressbookUrl: importForm.addressbookUrl,
        addressbookName: importForm.addressbookName,
        path: importForm.path,
      })
      await reloadContacts()
    } catch (e) {
      importError = formatError(e)
    } finally {
      importBusy = false
    }
  }

  function selectContact(id: string) {
    selectedId = id
    deleteConfirm = false
    formError = ''
    // Always land in read-only view when the user picks a
    // contact — explicit click on Edit toggles into the form.
    editing = false
    const c = contacts.find((x) => x.id === id)
    if (!c) return
    formName = c.display_name
    formEmails = c.email.map((e) => ({ ...e }))
    formPhones = c.phone.map((p) => ({ ...p }))
    formOrg = c.organization ?? ''
    formTitle = c.title ?? ''
    formBirthday = c.birthday ?? ''
    formNote = c.note ?? ''
    formWebsites = (c.urls ?? []).map((u) => ({ value: u }))
    formKeys = (c.keys ?? []).map((k) => ({ value: k }))
    formAddresses = (c.addresses ?? []).map((a) => ({ ...a }))
    selectedPhotoBytes = null
    photoRemoved = false
    formPhotoMime = c.photo_mime ?? null
    // We still need the bytes (not just a URL) so save can re-emit
    // them in the vCard — without this, an edit drops the avatar.
    if (c.photo_mime) void loadSelectedPhotoBytes(id)
    // ── #143 ───────────────────────────────────────────────
    formFamily = c.structured_name?.family ?? ''
    formGiven = c.structured_name?.given ?? ''
    formAdditional = c.structured_name?.additional ?? ''
    formPrefix = c.structured_name?.prefix ?? ''
    formSuffix = c.structured_name?.suffix ?? ''
    formNickname = c.nickname ?? ''
    formAnniversary = c.anniversary ?? ''
    formGender = c.gender ?? ''
    formRole = c.role ?? ''
    formTimezone = c.timezone ?? ''
    formImpp = (c.impp ?? []).map((i) => ({ ...i }))
    formLanguages = [...(c.languages ?? [])]
    formCategories = [...(c.categories ?? [])]
    formLanguageDraft = ''
    formCategoryDraft = ''
  }

  function startNew() {
    selectedId = 'new'
    deleteConfirm = false
    formError = ''
    // Creation always opens the editable form.
    editing = true
    formName = ''
    formEmails = []
    formPhones = []
    formOrg = ''
    formTitle = ''
    formBirthday = ''
    formNote = ''
    formWebsites = []
    formKeys = []
    formAddresses = []
    selectedPhotoBytes = null
    photoRemoved = false
    formPhotoMime = null
    // ── #143 ───────────────────────────────────────────────
    formFamily = ''
    formGiven = ''
    formAdditional = ''
    formPrefix = ''
    formSuffix = ''
    formNickname = ''
    formAnniversary = ''
    formGender = ''
    formRole = ''
    formTimezone = ''
    formImpp = []
    formLanguages = []
    formLanguageDraft = ''
    formCategories = []
    formCategoryDraft = ''
    if (!formAccountId && accounts.length > 0) {
      formAccountId = accounts[0].id
    }
    if (formAccountId) void loadAddressbooksFor(formAccountId)
  }

  /** Add a blank address row. Defaults to "home" so the picker has
      something selected — RFC 6350's TYPE param is optional but
      Nextcloud Contacts always groups by it, so we may as well too. */
  function addAddress() {
    formAddresses = [
      ...formAddresses,
      {
        kind: 'home',
        street: '',
        locality: '',
        region: '',
        postal_code: '',
        country: '',
      },
    ]
  }

  function removeAddress(idx: number) {
    formAddresses = formAddresses.filter((_, i) => i !== idx)
  }

  /** Add a blank phone row. Defaults to "cell" — by far the most
      common kind for a freshly-added number on a personal contact. */
  function addPhone() {
    formPhones = [...formPhones, { kind: 'cell', value: '' }]
  }

  function removePhone(idx: number) {
    formPhones = formPhones.filter((_, i) => i !== idx)
  }

  /** Add a blank email row. Defaults to "home" — typical for a
      personal contact entry; the user can flip to Work / Other. */
  function addEmail() {
    formEmails = [...formEmails, { kind: 'home', value: '' }]
  }

  function removeEmail(idx: number) {
    formEmails = formEmails.filter((_, i) => i !== idx)
  }

  function cancelEdit() {
    formError = ''
    deleteConfirm = false
    selectedPhotoBytes = null
    if (selectedId === 'new') {
      // Creation cancel — drop back to the empty state.
      selectedId = null
      editing = false
    } else if (selectedId) {
      // Editing an existing contact — flip back to view mode
      // and re-hydrate the form fields from the cached row so
      // any in-progress (now-discarded) changes don't bleed
      // into the next edit.
      const id = selectedId
      editing = false
      selectContact(id)
    } else {
      editing = false
    }
  }

  /** Switch from view mode into the edit form for the
   *  currently-selected contact.  Wired to the Edit button in
   *  the read-only view's top-right corner. */
  function startEdit() {
    if (!selectedId || selectedId === 'new') return
    editing = true
    formError = ''
    deleteConfirm = false
  }

  /**
   * Esc handler for the contact-editor side panel (#192).  Wired
   * via `<svelte:window onkeydown>` in the template.  Inert
   * while `saving` is in flight (mid-CardDAV PUT) so the user
   * can't bail and end up with an indeterminate save state, and
   * inert if a popover (`role="listbox"` autocomplete or one of
   * the inline emoji / kind menus) is open so it owns Esc.
   */
  function onContactsKeydown(e: KeyboardEvent) {
    if (e.key !== 'Escape') return
    // The import modal sits above everything — Escape closes it
    // first (unless an import is mid-flight) and never falls
    // through to the edit form underneath.
    if (importForm) {
      if (!importBusy) importForm = null
      e.preventDefault()
      return
    }
    if (selectedId === null) return
    if (saving) return
    if (document.querySelector('[role="listbox"]')) return
    e.preventDefault()
    cancelEdit()
  }

  // Pull just the bytes via IPC so we can round-trip them on save.
  // Display elsewhere uses `contactPhotoSrc()` against the URI scheme.
  async function loadSelectedPhotoBytes(id: string) {
    try {
      const photo = await api.contacts.getContactPhoto({ contactId: id })
      if (selectedId !== id) return
      selectedPhotoBytes = photo?.data ?? null
    } catch (e) {
      console.warn('get_contact_photo failed', e)
    }
  }

  // `contactPhotoSrc` (custom URI scheme → `<img src>`) and the
  // by-email lookup index both live on `contactsStore` so MailList
  // can use them too (#305).

  function onAccountChange() {
    void loadAddressbooksFor(formAccountId)
  }

  function onAddressbookChange(value: string) {
    const books = addressbooksByAccount[formAccountId] ?? []
    const picked = books.find((b) => b.path === value)
    formAddressbookUrl = value
    formAddressbookName = picked?.name ?? ''
  }

  function buildInput(): ContactInput {
    // The form now owns the photo MIME via `formPhotoMime`, which
    // is loaded from the cached contact on open (#143) and
    // overwritten by the click-to-upload pipeline.  Combined with
    // `selectedPhotoBytes`, that gives us a complete photo
    // payload regardless of whether the avatar came from the
    // server or from a fresh upload.
    //
    // Fold un-committed chip drafts in at save time — typing a
    // language / category and clicking Save without pressing
    // Enter would otherwise silently drop the value (#522).
    const languages = [...formLanguages]
    const langDraft = formLanguageDraft.trim()
    if (langDraft && !languages.some((l) => l.toLowerCase() === langDraft.toLowerCase())) {
      languages.push(langDraft)
    }
    const categories = [...formCategories]
    const catDraft = formCategoryDraft.trim()
    if (catDraft && !categories.some((c) => c.toLowerCase() === catDraft.toLowerCase())) {
      categories.push(catDraft)
    }
    return {
      display_name: formName.trim(),
      // Drop empty-value rows the same way phones do — an unfilled
      // "Add email" slot shouldn't ship to the server as a blank.
      emails: formEmails
        .filter((e) => e.value.trim())
        .map((e) => ({ kind: e.kind, value: e.value.trim() })),
      // Drop empty-value rows so an unfilled "Add phone" slot
      // doesn't end up as a blank entry on the server.
      phones: formPhones
        .filter((p) => p.value.trim())
        .map((p) => ({ kind: p.kind, value: p.value.trim() })),
      organization: formOrg.trim() || null,
      // Photo: bytes present = replace; `photo_mime: ''` with no
      // bytes = explicit removal (empty-string-clears contract);
      // both null = leave the server's photo alone (protects a
      // save that races the lazy photo-byte load).
      photo_mime: photoRemoved ? '' : formPhotoMime,
      photo_data: !photoRemoved && formPhotoMime ? selectedPhotoBytes : null,
      // The optional scalar fields below MUST send the empty string
      // (not null) when cleared: the Rust merge treats an explicit
      // empty string as "delete this property" but cannot tell JSON
      // null from an omitted key (serde maps both to None), and an
      // omitted key means "preserve whatever the server has" — so a
      // null here made clearing a saved note/title/etc. impossible
      // (#522).
      title: formTitle.trim(),
      birthday: formBirthday.trim(),
      note: formNote.trim(),
      urls: formWebsites
        .map((w) => w.value.trim())
        .filter((u) => u.length > 0),
      // #57 — OpenPGP keys round-trip through the vCard's `KEY:`
      // property.  Empty rows the user added but never filled in
      // are dropped here so they don't end up as phantom KEY lines
      // on the server.
      keys: formKeys
        .map((k) => k.value.trim())
        .filter((v) => v.length > 0),
      // Strip empty rows so the user can't end up with a phantom
      // address from forgetting to fill in the slots they added.
      addresses: formAddresses.filter(
        (a) =>
          a.street.trim() ||
          a.locality.trim() ||
          a.region.trim() ||
          a.postal_code.trim() ||
          a.country.trim(),
      ),
      // ── #143 ─────────────────────────────────────────────
      structured_name: {
        family: formFamily.trim(),
        given: formGiven.trim(),
        additional: formAdditional.trim(),
        prefix: formPrefix.trim(),
        suffix: formSuffix.trim(),
      },
      // Same empty-string-clears contract as title / birthday /
      // note above.
      nickname: formNickname.trim(),
      anniversary: formAnniversary.trim(),
      gender: formGender.trim(),
      role: formRole.trim(),
      timezone: formTimezone.trim(),
      impp: formImpp
        .filter((i) => i.value.trim())
        .map((i) => ({ kind: i.kind, value: i.value.trim() })),
      languages,
      categories,
    }
  }

  // ── #143: photo upload + chip-input helpers ─────────────────

  /** Click-to-upload pipeline.  Opens the OS file picker via
   *  `plugin-dialog`, reads the chosen image as bytes through
   *  `read_text_from_path`'s sibling Rust command for binary
   *  files (here we use the dialog filter to constrain to images
   *  and the FileReader API for the byte read).  Drops the bytes
   *  into `selectedPhotoBytes` and the MIME into `formPhotoMime`
   *  so the next save re-emits the avatar with the new image. */
  async function pickPhoto() {
    try {
      const picked = await api.platform.openFileDialog({
        multiple: false,
        directory: false,
        filters: [
          {
            name: 'Image',
            extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp'],
          },
        ],
      })
      if (!picked || Array.isArray(picked)) return
      // `picked` is the absolute path on disk.  We use the same
      // `save_bytes_to_path` sibling helper used elsewhere — but
      // for reading we go through `read_text_from_path`?  No —
      // that's text-only.  We need binary read; the simplest
      // path is to fetch via `convertFileSrc` and a fetch call.
      const url = api.platform.assetUrl(picked)
      const resp = await fetch(url)
      const bytes = new Uint8Array(await resp.arrayBuffer())
      // Sniff the MIME from the file extension since the
      // response Content-Type may not be set on the asset
      // protocol.  Defaults to JPEG which is the safest fallback
      // for NC's contact UI.
      const ext = picked.split('.').pop()?.toLowerCase() ?? 'jpg'
      const mime =
        ext === 'png'
          ? 'image/png'
          : ext === 'gif'
            ? 'image/gif'
            : ext === 'webp'
              ? 'image/webp'
              : 'image/jpeg'
      selectedPhotoBytes = Array.from(bytes)
      formPhotoMime = mime
      photoRemoved = false
    } catch (e) {
      console.warn('pickPhoto failed', e)
    }
  }

  /** Data URL for the freshly-picked photo, used by the form's
   *  avatar `<img>` so the user sees the new picture before
   *  save.  Falls back to the existing photoSrc when no fresh
   *  upload has happened yet. */
  function formAvatarSrc(): string | null {
    // After an explicit removal the cached photo must not bleed
    // back in through the fallback below — the preview showing
    // the old avatar made "Remove photo" look like a no-op.
    if (photoRemoved) return null
    if (selectedPhotoBytes && formPhotoMime) {
      // Build a data URL from the in-memory bytes.  Small enough
      // for typical avatars (a few hundred KB) that the base64
      // round-trip cost is negligible.
      const bin = new Uint8Array(selectedPhotoBytes)
      let s = ''
      for (let i = 0; i < bin.length; i++) s += String.fromCharCode(bin[i])
      return `data:${formPhotoMime};base64,${btoa(s)}`
    }
    if (selectedContact && contactPhotoSrc(selectedContact)) return contactPhotoSrc(selectedContact)
    return null
  }

  function commitChipDraft(
    list: string[],
    draft: string,
    onChange: (next: string[], nextDraft: string) => void,
  ) {
    const t = draft.trim()
    if (!t) {
      onChange(list, '')
      return
    }
    if (list.some((x) => x.toLowerCase() === t.toLowerCase())) {
      onChange(list, '')
      return
    }
    onChange([...list, t], '')
  }
  function removeChip(list: string[], idx: number, set: (next: string[]) => void) {
    set(list.filter((_, i) => i !== idx))
  }
  /** Category suggestion popover (#143 follow-up): clicking the
   *  category input shows the categories already in use across the
   *  user's other contacts, filtered as they type, so the user
   *  doesn't have to remember the exact spelling. */
  let categoryFieldFocused = $state(false)
  /** All distinct categories currently in use across the loaded
   *  contacts.  Sorted alphabetically for a stable suggestion
   *  list. */
  const allKnownCategories = $derived.by(() => {
    const set = new Set<string>()
    for (const c of contacts) {
      for (const cat of c.categories ?? []) {
        if (cat) set.add(cat)
      }
    }
    return [...set].sort((a, b) => a.localeCompare(b))
  })
  /** Subset of `allKnownCategories` shown in the popover: drops
   *  ones already attached to this contact, then case-insensitive-
   *  filters by whatever the user has typed in the chip draft. */
  const filteredCategorySuggestions = $derived.by(() => {
    const draft = formCategoryDraft.trim().toLowerCase()
    const taken = new Set(formCategories.map((c) => c.toLowerCase()))
    return allKnownCategories.filter(
      (cat) =>
        !taken.has(cat.toLowerCase()) &&
        (!draft || cat.toLowerCase().includes(draft)),
    )
  })

  function addImpp() {
    formImpp = [...formImpp, { kind: 'matrix', value: '' }]
  }
  function removeImpp(idx: number) {
    formImpp = formImpp.filter((_, i) => i !== idx)
  }
  function addWebsite() {
    formWebsites = [...formWebsites, { value: '' }]
  }
  function removeWebsite(idx: number) {
    formWebsites = formWebsites.filter((_, i) => i !== idx)
  }
  function addKey() {
    formKeys = [...formKeys, { value: '' }]
  }
  function removeKey(idx: number) {
    formKeys = formKeys.filter((_, i) => i !== idx)
  }

  // Shared option lists for the modern <Select> popover (#143).
  // Derived getters so paraglide-localised labels track the active
  // locale at render time.
  const emailKindOptions = $derived([
    { value: 'home', label: m.contact_form_kind_home() },
    { value: 'work', label: m.contact_form_kind_work() },
    { value: 'other', label: m.contact_form_kind_other() },
  ])
  const phoneKindOptions = $derived([
    { value: 'cell', label: m.contact_form_kind_mobile() },
    { value: 'work', label: m.contact_form_kind_work() },
    { value: 'home', label: m.contact_form_kind_home() },
    { value: 'fax', label: m.contact_form_kind_fax() },
    { value: 'other', label: m.contact_form_kind_other() },
  ])
  const imppKindOptions = $derived([
    { value: 'matrix', label: m.contact_form_kind_matrix() },
    { value: 'xmpp', label: m.contact_form_kind_xmpp() },
    { value: 'telegram', label: m.contact_form_kind_telegram() },
    { value: 'signal', label: m.contact_form_kind_signal() },
    { value: 'skype', label: m.contact_form_kind_skype() },
    { value: 'whatsapp', label: m.contact_form_kind_whatsapp() },
    { value: 'other', label: m.contact_form_kind_other() },
  ])
  const addressKindOptions = $derived([
    { value: 'home', label: m.contact_form_kind_home() },
    { value: 'work', label: m.contact_form_kind_work() },
    { value: 'other', label: m.contact_form_kind_other() },
  ])

  /** Human-readable label for a kind tag (`home`, `work`,
   *  `cell`, etc.).  Used by the read-only view to render
   *  per-row prefixes like "Home: jane@example.com".
   *  Falls back to a capitalised version of the raw tag for
   *  values we don't have a localised string for. */
  function kindLabel(kind: string): string {
    switch ((kind ?? '').toLowerCase()) {
      case 'home':
        return m.contact_form_kind_home()
      case 'work':
        return m.contact_form_kind_work()
      case 'other':
        return m.contact_form_kind_other()
      case 'cell':
      case 'mobile':
        return m.contact_form_kind_mobile()
      case 'fax':
        return m.contact_form_kind_fax()
      case 'matrix':
        return m.contact_form_kind_matrix()
      case 'xmpp':
        return m.contact_form_kind_xmpp()
      case 'telegram':
        return m.contact_form_kind_telegram()
      case 'signal':
        return m.contact_form_kind_signal()
      case 'skype':
        return m.contact_form_kind_skype()
      case 'whatsapp':
        return m.contact_form_kind_whatsapp()
      default:
        return kind ? kind.charAt(0).toUpperCase() + kind.slice(1) : ''
    }
  }
  /** Build the same family / given / etc.-joined string the
   *  Rust side derives at save time so view mode shows the
   *  exact heading the saved card carries.  When the contact
   *  has only `display_name` (no structured-name parts), this
   *  returns empty and callers fall back to `display_name`. */
  function structuredFullName(c: Contact): string {
    const sn = c.structured_name
    if (!sn) return ''
    const parts = [sn.prefix, sn.given, sn.additional, sn.family, sn.suffix]
      .map((p) => (p ?? '').trim())
      .filter((p) => p.length > 0)
    return parts.join(' ')
  }
  /** True when none of the fields in a logical "section"
   *  carry any value — used to skip empty sections in view
   *  mode so the layout stays tight. */
  function hasPersonalDetails(c: Contact): boolean {
    return !!(
      c.nickname?.trim()
      || c.birthday?.trim()
      || c.anniversary?.trim()
      || c.gender?.trim()
    )
  }
  function hasCommunicationDetails(c: Contact): boolean {
    return (
      (c.email?.length ?? 0) > 0
      || (c.phone?.length ?? 0) > 0
      || (c.impp?.length ?? 0) > 0
    )
  }
  function hasWorkDetails(c: Contact): boolean {
    return !!(
      c.organization?.trim()
      || c.title?.trim()
      || c.role?.trim()
      || (c.categories?.length ?? 0) > 0
    )
  }
  function hasAddressWebDetails(c: Contact): boolean {
    return (
      (c.addresses?.length ?? 0) > 0
      || (c.urls?.length ?? 0) > 0
    )
  }
  function hasOtherDetails(c: Contact): boolean {
    return !!(
      (c.languages?.length ?? 0) > 0
      || c.timezone?.trim()
      || c.note?.trim()
    )
  }
  function hasEncryptionDetails(c: Contact): boolean {
    return (c.keys?.length ?? 0) > 0
  }

  /** Best-effort upsert of every newly-pasted `KEY:` value into the
   *  recipient-crypto caches so Compose can find it without waiting
   *  for a CardDAV sync round-trip.
   *
   *  One vCard `KEY:` field carries either an OpenPGP key (#57) or an
   *  X.509 certificate (#338), so we try *both* importers per value
   *  and let each reject what isn't its format: `pgp_import_public_key`
   *  parses armored OpenPGP and errors on a PEM cert;
   *  `smime_import_public_cert` parses X.509 PEM/DER and errors on an
   *  OpenPGP block.  Whichever matches lands the value in its cache;
   *  the other's error is logged and ignored.  All failures are
   *  per-value and non-fatal — a malformed paste shouldn't roll back
   *  the contact edit, since the vCard write already succeeded.  The
   *  `email_hint` ties each entry to the contact's primary email so
   *  the Compose recipient lookup resolves it. */
  async function pushKeysToCryptoCache(contact: Contact) {
    const primaryEmail = contact.email[0]?.value ?? null
    for (const value of contact.keys ?? []) {
      try {
        await api.crypto.pgpImportPublicKey({
          armoredKey: value,
          emailHint: primaryEmail,
        })
      } catch (e) {
        console.warn('pgp_import_public_key failed for contact key', e)
      }
      try {
        await api.crypto.smimeImportPublicCert({
          certData: value,
          emailHint: primaryEmail,
        })
      } catch (e) {
        console.warn('smime_import_public_cert failed for contact key', e)
      }
    }
  }

  async function saveContact() {
    formError = ''
    const input = buildInput()
    if (!input.display_name) {
      formError = 'Please enter a name.'
      return
    }

    saving = true
    try {
      if (selectedId === 'new') {
        if (!formAccountId || !formAddressbookUrl || !formAddressbookName) {
          formError = 'Pick a Nextcloud account and addressbook first.'
          return
        }
        const created = await api.contacts.createContact({
          ncId: formAccountId,
          addressbookUrl: formAddressbookUrl,
          addressbookName: formAddressbookName,
          input,
        })
        await reloadContacts()
        // Land on the freshly-created contact in view mode so
        // the user can immediately see the saved record without
        // a stray editable form sticking around.
        selectContact(created.id)
        // #57 — best-effort push of any pasted PGP keys into the
        // recipient-key cache so Compose can encrypt to this
        // contact without waiting for the next CardDAV sync.
        void pushKeysToCryptoCache(created)
      } else if (selectedId) {
        const updated = await api.contacts.updateContact({
          contactId: selectedId,
          input,
        })
        await reloadContacts()
        // Re-select to refresh the view-mode display from the
        // cached row, then flip out of edit mode.
        selectContact(updated.id)
        void pushKeysToCryptoCache(updated)
      }
    } catch (e) {
      formError = formatError(e) || 'Failed to save contact'
    } finally {
      saving = false
    }
  }

  async function deleteSelected() {
    if (!selectedId || selectedId === 'new') return
    saving = true
    formError = ''
    try {
      await api.contacts.deleteContact({ contactId: selectedId })
      await reloadContacts()
      selectedId = null
      deleteConfirm = false
    } catch (e) {
      formError = formatError(e) || 'Failed to delete contact'
    } finally {
      saving = false
    }
  }

  function accountLabel(id: string): string {
    const a = accounts.find((x) => x.id === id)
    if (!a) return id
    return a.display_name ?? a.username
  }

  const selectedContact = $derived(
    selectedId && selectedId !== 'new'
      ? (contacts.find((c) => c.id === selectedId) ?? null)
      : null,
  )
</script>

<svelte:window onkeydown={onContactsKeydown} />

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Stacked header (#522) — the standard integration-view shell:
       title above its icon-only actions, docked LEFT so the
       controls stay in the viewing angle near the columns they act
       on.  No header search in this view — each tab's search sits
       in the middle column directly above the list it filters
       (contacts on the Contacts tab, members on the Lists tab), so
       both tabs share one pattern and the header never changes
       height. -->
  <div class="flex items-center gap-3 px-6 py-3 border-b glass-panel">
    <div class="flex-1 min-w-0 flex flex-col items-start gap-2">
      <h2 class="text-xl font-semibold truncate">{m.contacts_view_title()}</h2>
      <div class="flex items-center gap-2 shrink-0">
      <button
        class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
        disabled={accounts.length === 0}
        onclick={startNew}
        title={m.contact_form_new_heading()}
        aria-label={m.contact_form_new_heading()}
      ><Icon name="plus" size={14} /></button>
      <button
        class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"
        disabled={accounts.length === 0}
        onclick={openImport}
        title={m.contact_import_button()}
        aria-label={m.contact_import_button()}
      ><Icon name="upload" size={14} /></button>
      <button
        class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"
        disabled={accounts.length === 0 || syncing}
        onclick={() => void syncInBackground()}
        title={syncing ? m.contacts_view_syncing() : m.contacts_view_sync_title()}
        aria-label={syncing ? m.contacts_view_syncing() : m.contacts_view_sync()}
      ><Icon name={syncing ? 'loading' : 'sync'} size={14} /></button>
      </div>
    </div>
  </div>

  <div class="flex-1 min-h-0 flex">
  <!-- ── Sidebar — tab strip and (when on the Contacts tab)
       addressbooks + Contact Groups.  The sidebar stays mounted
       on the Mailing lists tab so the tab strip doesn't move;
       only the navigation sections collapse. ────────────────── -->
  <aside
    class="shrink-0 border-r glass-panel flex flex-col"
    use:resizableSidebar={{ key: 'contacts.navSidebar', defaultWidth: 224, min: 160, max: 480 }}
  >
    <!-- Tab strip.  New contact / Import / Sync moved to the view
         header's action slot (#522) so the sidebar starts with
         navigation, like the other integration views.  Buttons
         explicitly stop propagation + set activeTab on a separate
         handler so any pending document-level click-outside
         listener can't race the state update on the first
         transition. -->
    <div class="px-3 pt-3 flex gap-1">
      <button
        type="button"
        class="flex-1 px-2 py-2 text-sm rounded-lg transition-colors {activeTab === 'contacts'
          ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
          : 'text-surface-600 dark:text-surface-300 hover:bg-primary-500/10'}"
        onclick={() => selectTab('contacts')}
      >Contacts</button>
      <button
        type="button"
        class="flex-1 px-2 py-2 text-sm rounded-lg transition-colors {activeTab === 'lists'
          ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
          : 'text-surface-600 dark:text-surface-300 hover:bg-primary-500/10'}"
        onclick={() => selectTab('lists')}
      >Lists</button>
    </div>
    {#if activeTab === 'contacts'}
    <div class="flex-1 overflow-y-auto px-2 py-3 space-y-1">
      <!-- "All" -->
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-left transition-colors {selectedScope === 'all'
          ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
          : 'hover:bg-primary-500/10'}"
        onclick={() => (selectedScope = 'all')}
      >
        <span class="w-6 flex items-center justify-center"><Icon name="meetings" size={16} /></span>
        <span class="flex-1 truncate">All contacts</span>
        <span class="text-xs text-surface-500">{contacts.length}</span>
      </button>

      <!-- Addressbooks — one row per CardDAV collection.  Click
           filters the middle list to entries from that book's
           NC account (the contact row doesn't carry the
           addressbook path, so we approximate by NC account). -->
      {#if allAddressbooks.length > 0}
        <div class="px-3 pt-3 pb-1 text-[10px] uppercase tracking-wider text-surface-500">
          Addressbooks
        </div>
        {#each allAddressbooks as b (`${b.ncId}::${b.name}`)}
          {@const sel = selectedScope === `addressbook:${b.name}`}
          <button
            class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-left transition-colors
                   {sel
                     ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
                     : 'hover:bg-primary-500/10'}"
            onclick={() => (selectedScope = `addressbook:${b.name}`)}
          >
            <span class="w-6 flex items-center justify-center"><Icon name="address-book" size={16} /></span>
            <span class="flex-1 truncate">{b.displayName ?? b.name}</span>
          </button>
        {/each}
      {/if}

      <!-- Contact Groups — derived from CATEGORIES on every
           cached vCard.  Drag-drop a contact onto a row to add
           it; right-click opens rename / delete; the swatch
           toggles "Use as mailing list". -->
      <div class="px-3 pt-3 pb-1 flex items-center justify-between">
        <span class="text-[10px] uppercase tracking-wider text-surface-500">Contact Groups</span>
        <button
          class="w-5 h-5 rounded-lg flex items-center justify-center text-surface-500 hover:bg-primary-500/10"
          title="New Contact Group"
          aria-label="New Contact Group"
          onclick={() => void createCategory()}
        >+</button>
      </div>
      {#each categories as c (c.name)}
        {@const sel = selectedScope === `category:${c.name}`}
        {@const dragOver = dragHoverCategory === c.name}
        <!-- Container is a div, not a button, so the inline
             "Use as mailing list" swatch can stay a real
             <button> — nested <button> would otherwise trip
             the HTML parser's repair pass.  We add
             role="button" + tabindex + keyboard handler so the
             a11y story matches a regular button. -->
        <div
          role="button"
          tabindex="0"
          class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-left transition-colors cursor-pointer
                 {sel
                   ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
                   : 'hover:bg-primary-500/10'}
                 {dragOver ? 'ring-2 ring-primary-500' : ''}"
          oncontextmenu={(e) => {
            e.preventDefault()
            const p = cursorAnchor(e)
            menuTop = p.y
            menuLeft = p.x
            openMenuFor = `cat:${c.name}`
          }}
          ondragover={(e) => {
            if (!draggedContactId) return
            e.preventDefault()
            dragHoverCategory = c.name
          }}
          ondragleave={() => {
            if (dragHoverCategory === c.name) dragHoverCategory = null
          }}
          ondrop={(e) => {
            e.preventDefault()
            const id = draggedContactId
            dragHoverCategory = null
            draggedContactId = null
            if (id) void addContactIdToCategory(id, c.name)
          }}
          onclick={() => (selectedScope = `category:${c.name}`)}
          onkeydown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault()
              selectedScope = `category:${c.name}`
            }
          }}
        >
          <span class="w-6 text-center">{(c.name || '?').slice(0, 1).toUpperCase()}</span>
          <span class="flex-1 truncate">{c.name}</span>
          <span class="text-xs text-surface-500 mr-1">{c.memberCount}</span>
          <!-- Three-dot trigger: rename / delete.  The menu itself
               renders at the component root (see the bottom of the
               template) — the glass sidebar's backdrop-filter makes
               this aside the containing block for position:fixed
               descendants, so a menu nested here would be offset by
               the aside's own viewport position. -->
          <button
            class="w-5 h-5 rounded shrink-0 text-surface-500 hover:bg-surface-300 dark:hover:bg-surface-600 leading-none"
            title="More actions"
            aria-label="Contact Group actions"
            onclick={(e) => {
              e.stopPropagation()
              const r = anchorRect(e.currentTarget as HTMLElement)
              menuTop = r.top
              menuLeft = r.right + 6
              openMenuFor = openMenuFor === `cat:${c.name}` ? null : `cat:${c.name}`
            }}
          >⋯</button>
        </div>
      {/each}
      {#if categories.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500 italic">
          No Contact Groups yet. Click <span class="font-semibold">+</span> to
          create one — drag contacts onto it after to tag them.
        </p>
      {/if}
    </div>
    {/if}
    {#if activeTab === 'lists'}
    <!-- Lists tab — sidebar shows three sections (Mailing
         lists / Contact Groups / Teams).  Split out from the
         {#if/:else} pair as two independent branches because
         the {:else} variant was being clipped by the open
         three-dot menu's fixed overlay on first click,
         leaving activeTab updated but visually stale. -->
    <div class="flex-1 overflow-y-auto px-2 py-3 space-y-1">
      {#snippet listRow(ml: MailingListView, sourceIconName: IconName, _pillCls: string, _pillText: string)}
        {@const sel = selectedListId === ml.id}
        {@const hidden = ml.hiddenFromAutocomplete}
        <div class="relative">
          <div
            role="button"
            tabindex="0"
            class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm cursor-pointer transition-colors {sel
              ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
              : 'hover:bg-primary-500/10'}"
            onclick={() => (selectedListId = ml.id)}
            onkeydown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault()
                selectedListId = ml.id
              }
            }}
            oncontextmenu={(e) => {
              if (ml.source === 'team') return
              e.preventDefault()
              const p = cursorAnchor(e)
              menuTop = p.y
              menuLeft = p.x
              openMenuFor = `ml:${ml.id}`
            }}
          >
            <!-- Left swatch toggles hide-from-autocomplete, mirroring
                 the calendar sidebar's mute swatch. Filled = used in
                 autocomplete, outlined = suppressed. -->
            <button
              class="w-3 h-3 rounded-sm shrink-0 border border-primary-500 transition-colors cursor-pointer {hidden ? 'bg-transparent' : 'bg-primary-500'}"
              title={hidden ? 'Show in autocomplete' : 'Hide from autocomplete'}
              aria-label={hidden ? 'Show in autocomplete' : 'Hide from autocomplete'}
              onclick={(e) => {
                e.stopPropagation()
                if (ml.source === 'category') {
                  void toggleCategoryAsList(ml.name, !hidden)
                } else {
                  void toggleMailingListHidden(ml.id, hidden)
                }
              }}
            ></button>
            <span class="w-5 flex items-center justify-center">
              {#if ml.emoji}
                {ml.emoji}
              {:else}
                <Icon name={sourceIconName} size={14} />
              {/if}
            </span>
            {#if renamingListId === ml.id}
              <!-- svelte-ignore a11y_autofocus -->
              <input
                type="text"
                class="input flex-1 text-sm px-2 py-0.5 rounded min-w-0"
                bind:value={renameValue}
                autofocus
                onclick={(e) => e.stopPropagation()}
                onkeydown={(e) => {
                  e.stopPropagation()
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    void commitRenameMailingList(ml)
                  } else if (e.key === 'Escape') {
                    e.preventDefault()
                    renamingListId = null
                  }
                }}
                onblur={() => void commitRenameMailingList(ml)}
              />
            {:else}
              <span class="flex-1 truncate {hidden ? 'text-surface-400 dark:text-surface-500' : ''}">{ml.name}</span>
            {/if}
            <span class="text-xs text-surface-500">{ml.members.filter((m) => m.email).length}</span>
            {#if ml.source !== 'team'}
              <button
                class="w-5 h-5 rounded text-surface-500 hover:bg-surface-300 dark:hover:bg-surface-600 leading-none shrink-0"
                title="More actions"
                aria-label="Mailing list actions"
                onclick={(e) => {
                  e.stopPropagation()
                  const r = anchorRect(e.currentTarget as HTMLElement)
                  menuTop = r.top
                  menuLeft = r.right + 6
                  openMenuFor = openMenuFor === `ml:${ml.id}` ? null : `ml:${ml.id}`
                }}
              >⋯</button>
            {/if}
          </div>
          <!-- The three-dot menu + emoji picker render at the
               component root (see the bottom of the template) so
               their position:fixed anchoring escapes the glass
               sidebar's backdrop-filter containing block. -->
        </div>
      {/snippet}

      <!-- Mailing lists — the manual KIND:group cards.  Header
           gets its own `+` so creating one feels symmetric with
           the Contact Groups section in the Contacts tab. -->
      <div class="px-3 pt-1 pb-1 flex items-center justify-between">
        <span class="text-[10px] uppercase tracking-wider text-surface-500">Mailing lists</span>
        <button
          class="w-5 h-5 rounded-lg flex items-center justify-center text-surface-500 hover:bg-primary-500/10"
          title="New mailing list"
          aria-label="New mailing list"
          onclick={() => openNewMailingListForm()}
        >+</button>
      </div>
      {#each filteredMailingLists.manual as ml (ml.id)}
        {@render listRow(ml, 'email-envelope', 'bg-success-500/20 text-success-600 dark:text-success-300', 'manual')}
      {/each}
      {#if filteredMailingLists.manual.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500 italic">
          None yet. Click <span class="font-semibold">+</span> to create one.
        </p>
      {/if}
      {#if filteredMailingLists.category.length > 0}
        <div class="px-3 pt-3 pb-1 text-[10px] uppercase tracking-wider text-surface-500">Contact Groups</div>
        {#each filteredMailingLists.category as ml (ml.id)}
          {@render listRow(ml, 'group', 'bg-primary-500/20 text-primary-600 dark:text-primary-300', 'category')}
        {/each}
      {/if}
      {#if filteredMailingLists.team.length > 0}
        <div class="px-3 pt-3 pb-1 text-[10px] uppercase tracking-wider text-surface-500">Teams</div>
        {#each filteredMailingLists.team as ml (ml.id)}
          {@render listRow(ml, 'team', 'bg-surface-300 dark:bg-surface-600 text-surface-700 dark:text-surface-200', 'team')}
        {/each}
      {/if}
      {#if mailingLists.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500 italic">
          No mailing lists yet. Click <span class="font-semibold">+ New mailing list</span> for
          a manual one, or tag contacts with a Contact Group and pick
          "Use as mailing list" from its three-dot menu.
        </p>
      {/if}
    </div>
    {/if}
  </aside>

  <!-- ── Middle column: contact list / mailing-list catalogue.
       The shell heading + tab strip moved into the sidebar
       above, so this column's job is just the list itself. ─ -->
  <aside
    class="shrink-0 border-r border-surface-200 dark:border-surface-700 flex flex-col"
    use:resizableSidebar={{ key: 'contacts.listColumn', defaultWidth: 320, min: 240, max: 600 }}
  >
    {#if activeTab === 'contacts'}
    <!-- Search sits directly above the list it filters — same
         placement as the Lists tab's member search, so the two
         tabs share one pattern (#522). -->
    <div class="border-b border-surface-200 dark:border-surface-700 p-2">
      <SearchInput bind:value={query} placeholder={m.contacts_view_search_placeholder()} />
    </div>

    <div class="flex-1 overflow-y-auto pb-3">
      {#if loading}
        <p class="px-3 py-2 text-xs text-surface-500">Loading contacts…</p>
      {:else if error}
        <p class="px-3 py-2 text-xs text-red-500">{error}</p>
      {:else if contacts.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500">
          {m.contacts_view_empty()}
        </p>
      {:else if filteredContacts.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500">No matches for “{query}”.</p>
      {:else}
        {#each filteredContacts as c (c.id)}
          <button
            class="w-full flex items-center gap-2 px-3 py-2 text-sm transition-colors border-b border-surface-100 dark:border-surface-800
              {selectedId === c.id
                ? 'bg-primary-500/10 text-primary-500 font-medium'
                : 'hover:bg-primary-500/10'}"
            draggable="true"
            ondragstart={(e) => {
              draggedContactId = c.id
              e.dataTransfer?.setData('text/plain', c.display_name)
              if (e.dataTransfer) e.dataTransfer.effectAllowed = 'copy'
            }}
            ondragend={() => {
              draggedContactId = null
              dragHoverCategory = null
            }}
            onclick={() => selectContact(c.id)}
          >
            <Avatar
              photo={contactPhotoSrc(c)}
              displayName={c.display_name}
              seed={c.email[0]?.value ?? c.id}
              size={32}
            />
            <span class="flex flex-col min-w-0 text-left">
              <span class="truncate">{c.display_name || '(no name)'}</span>
              {#if c.email.length > 0}
                <span class="text-xs text-surface-500 truncate normal-case">{c.email[0].value}</span>
              {/if}
            </span>
          </button>
        {/each}
      {/if}
    </div>
    {/if}
    {#if activeTab === 'lists'}
    <!-- Lists tab — middle column shows the SELECTED list's
         members (or a hint if nothing's selected).  All
         management (create / rename / hide / delete) happens
         from the sidebar's three-dot menus. -->
    {#if !selectedList}
      <div class="flex-1 flex items-center justify-center text-surface-500 text-sm p-6 text-center">
        Pick a mailing list on the left to see its members.
      </div>
    {:else}
      {@const ml = selectedList}
      {@const editable = ml.source === 'manual' || ml.source === 'category'}
      {@const memberEmails = new Set(ml.members.map((m) => m.email.toLowerCase()).filter(Boolean))}
      {@const filteredMembers = memberQuery.trim()
        ? ml.members.filter((m) =>
            m.displayName.toLowerCase().includes(memberQuery.trim().toLowerCase()) ||
            m.email.toLowerCase().includes(memberQuery.trim().toLowerCase()),
          )
        : ml.members}
      {@const pickableContacts = pickerOpen
        ? contacts.filter((c) => {
            // Hide contacts already in the list.
            if (c.email.some((e) => memberEmails.has(e.value.toLowerCase()))) return false
            const q = pickerQuery.trim().toLowerCase()
            if (!q) return true
            return (
              c.display_name.toLowerCase().includes(q) ||
              c.email.some((e) => e.value.toLowerCase().includes(q))
            )
          })
        : []}
      <div class="flex-1 flex flex-col overflow-hidden">
        <!-- Search row — mirrors the Contacts tab: search directly
             above the list it filters, no list-name heading (the
             sidebar's selected row already says which list is
             open).  `pickerOpen` swaps the bound query so the same
             input alternates between filtering members and
             filtering the contact-picker pool; the icon button
             beside it toggles the picker — `plus` (filled-primary
             CTA) to start adding, `close` (tonal) to finish. -->
        <div class="border-b border-surface-200 dark:border-surface-700 p-2 flex items-center gap-2">
          {#if pickerOpen}
            <SearchInput
              bind:value={pickerQuery}
              placeholder={m.contacts_view_search_add_placeholder()}
              class="flex-1 min-w-0"
            />
          {:else}
            <SearchInput
              bind:value={memberQuery}
              placeholder={m.contacts_view_search_members_placeholder()}
              class="flex-1 min-w-0"
            />
          {/if}
          {#if editable}
            <button
              class="btn btn-sm {pickerOpen
                ? 'preset-tonal-surface'
                : 'preset-filled-primary-500'} inline-flex items-center justify-center shrink-0"
              title={pickerOpen ? m.contacts_view_add_member_done() : m.contacts_view_add_member()}
              aria-label={pickerOpen ? m.contacts_view_add_member_done() : m.contacts_view_add_member()}
              onclick={() => {
                pickerOpen = !pickerOpen
                pickerQuery = ''
              }}
            ><Icon name={pickerOpen ? 'close' : 'plus'} size={14} /></button>
          {/if}
        </div>
        <div class="flex-1 overflow-y-auto pb-3">
          {#if pickerOpen}
            {#if pickableContacts.length === 0}
              <p class="px-3 py-2 text-xs text-surface-500 italic">
                {pickerQuery.trim()
                  ? `No matches for "${pickerQuery}".`
                  : 'Every contact is already in this list.'}
              </p>
            {/if}
            {#each pickableContacts as c (c.id)}
              <button
                class="w-full flex items-center gap-2 px-3 py-2 text-sm text-left transition-colors border-b border-surface-100 dark:border-surface-800 hover:bg-primary-500/10"
                onclick={() => void addContactToSelectedList(c.id)}
              >
                <Avatar
                  photo={contactPhotoSrc(c)}
                  displayName={c.display_name}
                  seed={c.email[0]?.value ?? c.id}
                  size={28}
                />
                <div class="flex-1 min-w-0">
                  <p class="font-medium truncate">{c.display_name || '(no name)'}</p>
                  {#if c.email.length > 0}
                    <p class="text-xs text-surface-500 truncate normal-case">{c.email[0].value}</p>
                  {/if}
                </div>
                <span class="text-xs text-primary-500">+ Add</span>
              </button>
            {/each}
          {:else}
            {#if filteredMembers.length === 0}
              <p class="px-3 py-2 text-xs text-surface-500 italic">
                {memberQuery.trim()
                  ? `No matches for "${memberQuery}".`
                  : 'No members yet.'}
              </p>
            {/if}
            {#each filteredMembers as m, i (`${m.email}::${i}`)}
              {@const linkedContact = m.email ? contactsStore.byEmail.get(m.email.toLowerCase()) : undefined}
              {@const memberPhoto = linkedContact ? contactPhotoSrc(linkedContact) : null}
              {@const isOpenable = !!linkedContact}
              <!-- Members that resolve to an in-cache contact open
                   the right pane on click — same affordance as
                   selecting from the Contacts tab.  Members that
                   are just an email (no contact card) stay
                   non-clickable so the cursor doesn't lie about
                   what's interactive.  Wrapper stays a `<div>`
                   because it nests the Remove `<button>` and we
                   can't put `<button>` inside `<button>`. -->
              <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
              <div
                class="group flex items-center gap-2 px-3 py-2 text-sm transition-colors border-b border-surface-100 dark:border-surface-800 {isOpenable
                  ? 'cursor-pointer hover:bg-primary-500/10'
                  : ''} {linkedContact && selectedId === linkedContact.id
                  ? 'bg-primary-500/10 text-primary-500 font-medium'
                  : ''}"
                role={isOpenable ? 'button' : undefined}
                tabindex={isOpenable ? 0 : undefined}
                onclick={() => {
                  if (linkedContact) selectContact(linkedContact.id)
                }}
                onkeydown={(e) => {
                  if (!linkedContact) return
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault()
                    selectContact(linkedContact.id)
                  }
                }}
              >
                <Avatar
                  photo={memberPhoto}
                  displayName={m.displayName || m.email || '?'}
                  seed={m.email || m.displayName}
                  size={28}
                />
                <div class="flex-1 min-w-0">
                  <p class="font-medium truncate">{m.displayName || m.email || '(unnamed)'}</p>
                  <p class="text-xs text-surface-500 truncate">
                    {m.email || '(no email)'}
                  </p>
                </div>
                {#if editable && m.email}
                  <button
                    class="opacity-0 group-hover:opacity-100 transition-opacity w-7 h-7 rounded-lg text-surface-500 hover:bg-error-500/15 hover:text-error-500 leading-none shrink-0"
                    title="Remove from list"
                    aria-label="Remove from list"
                    onclick={(e) => {
                      e.stopPropagation()
                      void removeContactFromSelectedList(m.email)
                    }}
                  >×</button>
                {/if}
              </div>
            {/each}
          {/if}
        </div>
      </div>
    {/if}
    {/if}
  </aside>

  <!-- ── Right: detail / edit pane ──────────────────────────── -->
  <main class="flex-1 flex flex-col overflow-y-auto">
    {#if selectedId === null}
      <div class="flex-1 flex items-center justify-center text-surface-500 text-sm">
        {m.contacts_view_pick_hint()}
      </div>
    {:else if !editing && selectedId !== 'new' && selectedContact}
      <!-- ── Read-only view mode (#143 follow-up) ─────────────
           Default landing for any selected contact.  Renders the
           saved card as labels + values, omitting any field that
           has no content so the layout stays tight.  Edit / Delete
           live in the top-right corner; clicking Edit toggles the
           `editing` flag and the form template below takes over. -->
      <div class="max-w-2xl w-full mx-auto p-6 flex flex-col gap-5">
        <div class="flex items-start gap-4">
          <Avatar
            photo={contactPhotoSrc(selectedContact)}
            displayName={selectedContact.display_name || '?'}
            seed={selectedContact.email[0]?.value ?? selectedContact.id}
            size={80}
          />
          <div class="flex flex-col flex-1 min-w-0">
            <h3 class="text-xl font-semibold truncate">
              {structuredFullName(selectedContact) || selectedContact.display_name || m.contact_form_no_name()}
            </h3>
            {#if selectedContact.nickname}
              <span class="text-sm text-surface-500 truncate">"{selectedContact.nickname}"</span>
            {/if}
            <span class="text-xs text-surface-500 truncate mt-1">
              {m.contact_form_from_account({ account: accountLabel(selectedContact.nextcloud_account_id) })}
            </span>
          </div>
          <!-- Icon-only Edit / Delete pair — the EncryptionSettings
               title-row shape: identical outlined base for both,
               red hover overlay marking Delete as destructive.
               The delete-confirm branch keeps its labelled buttons
               deliberately — a destructive confirm should read as
               words, not glyphs. -->
          <div class="flex items-center gap-2 shrink-0">
            <button
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
              onclick={startEdit}
              title={m.contact_view_button_edit()}
              aria-label={m.contact_view_button_edit()}
            ><Icon name="compose" size={14} /></button>
            {#if deleteConfirm}
              <span class="text-xs text-surface-500">Really delete?</span>
              <button
                class="btn btn-sm preset-filled-error-500"
                onclick={deleteSelected}
                disabled={saving}
              >Confirm</button>
              <button
                class="btn btn-sm preset-tonal"
                onclick={() => (deleteConfirm = false)}
                disabled={saving}
              >Keep</button>
            {:else}
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                onclick={() => (deleteConfirm = true)}
                title={m.contact_view_button_delete()}
                aria-label={m.contact_view_button_delete()}
              ><Icon name="trash" size={14} /></button>
            {/if}
          </div>
        </div>

        {#if formError}
          <p class="text-sm text-red-500">{formError}</p>
        {/if}

        {#if hasPersonalDetails(selectedContact)}
          <section class="contact-view-section">
            <h4 class="contact-view-section-title">{m.contact_form_section_personal()}</h4>
            <dl class="contact-view-grid">
              {#if selectedContact.nickname}
                <dt>{m.contact_form_label_nickname()}</dt><dd>{selectedContact.nickname}</dd>
              {/if}
              {#if selectedContact.birthday}
                <dt>{m.contact_form_label_birthday()}</dt><dd>{selectedContact.birthday}</dd>
              {/if}
              {#if selectedContact.anniversary}
                <dt>{m.contact_form_label_anniversary()}</dt><dd>{selectedContact.anniversary}</dd>
              {/if}
              {#if selectedContact.gender}
                <dt>{m.contact_form_label_gender()}</dt><dd>{selectedContact.gender}</dd>
              {/if}
            </dl>
          </section>
        {/if}

        {#if hasCommunicationDetails(selectedContact)}
          <section class="contact-view-section">
            <h4 class="contact-view-section-title">{m.contact_form_section_communication()}</h4>
            {#if (selectedContact.email?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_emails()}</span>
                {#each selectedContact.email as e (e.value)}
                  <div class="contact-view-row">
                    <span class="contact-view-row-kind">{kindLabel(e.kind)}</span>
                    <a class="contact-view-row-value text-primary-600 dark:text-primary-300 hover:underline truncate" href="mailto:{e.value}">{e.value}</a>
                  </div>
                {/each}
              </div>
            {/if}
            {#if (selectedContact.phone?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_phones()}</span>
                {#each selectedContact.phone as p (p.value)}
                  <div class="contact-view-row">
                    <span class="contact-view-row-kind">{kindLabel(p.kind)}</span>
                    <span class="contact-view-row-value">{p.value}</span>
                  </div>
                {/each}
              </div>
            {/if}
            {#if (selectedContact.impp?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_impp()}</span>
                {#each selectedContact.impp ?? [] as im (im.value)}
                  <div class="contact-view-row">
                    <span class="contact-view-row-kind">{kindLabel(im.kind)}</span>
                    <span class="contact-view-row-value truncate">{im.value}</span>
                  </div>
                {/each}
              </div>
            {/if}
          </section>
        {/if}

        {#if hasWorkDetails(selectedContact)}
          <section class="contact-view-section">
            <h4 class="contact-view-section-title">{m.contact_form_section_work()}</h4>
            <dl class="contact-view-grid">
              {#if selectedContact.organization}
                <dt>{m.contact_form_label_organization()}</dt><dd>{selectedContact.organization}</dd>
              {/if}
              {#if selectedContact.title}
                <dt>{m.contact_form_label_job_title()}</dt><dd>{selectedContact.title}</dd>
              {/if}
              {#if selectedContact.role}
                <dt>{m.contact_form_label_role()}</dt><dd>{selectedContact.role}</dd>
              {/if}
            </dl>
            {#if (selectedContact.categories?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_categories()}</span>
                <div class="flex flex-wrap gap-1.5">
                  {#each selectedContact.categories ?? [] as cat (cat)}
                    <span class="inline-flex items-center px-2 py-0.5 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300 text-xs">
                      {cat}
                    </span>
                  {/each}
                </div>
              </div>
            {/if}
          </section>
        {/if}

        {#if hasAddressWebDetails(selectedContact)}
          <section class="contact-view-section">
            <h4 class="contact-view-section-title">{m.contact_form_section_address_web()}</h4>
            {#if (selectedContact.addresses?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_addresses()}</span>
                {#each selectedContact.addresses ?? [] as a, i (i)}
                  <div class="contact-view-address">
                    <span class="contact-view-row-kind">{kindLabel(a.kind)}</span>
                    <div class="contact-view-address-lines">
                      {#if a.street}<div>{a.street}</div>{/if}
                      {#if a.locality || a.region || a.postal_code}
                        <div>{[a.postal_code, a.locality, a.region].filter((s) => s?.trim()).join(' ')}</div>
                      {/if}
                      {#if a.country}<div>{a.country}</div>{/if}
                    </div>
                  </div>
                {/each}
              </div>
            {/if}
            {#if (selectedContact.urls?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_websites()}</span>
                {#each selectedContact.urls ?? [] as url (url)}
                  <div class="contact-view-row">
                    <a class="contact-view-row-value text-primary-600 dark:text-primary-300 hover:underline truncate" href={url} target="_blank" rel="noopener noreferrer">{url}</a>
                  </div>
                {/each}
              </div>
            {/if}
          </section>
        {/if}

        {#if hasEncryptionDetails(selectedContact)}
          <section class="contact-view-section">
            <h4 class="contact-view-section-title">{m.contact_form_section_encryption()}</h4>
            <div class="contact-view-block">
              <span class="contact-view-block-label">{m.contact_form_label_pgp_keys()}</span>
              {#each selectedContact.keys ?? [] as key, i (i)}
                <div class="contact-view-row">
                  <span class="contact-view-row-value font-mono text-xs break-all truncate">
                    {key.length > 80 ? key.slice(0, 80) + '…' : key}
                  </span>
                </div>
              {/each}
            </div>
          </section>
        {/if}

        {#if hasOtherDetails(selectedContact)}
          <section class="contact-view-section">
            <h4 class="contact-view-section-title">{m.contact_form_section_other()}</h4>
            {#if (selectedContact.languages?.length ?? 0) > 0}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_languages()}</span>
                <div class="flex flex-wrap gap-1.5">
                  {#each selectedContact.languages ?? [] as lang (lang)}
                    <span class="inline-flex items-center px-2 py-0.5 rounded-full bg-surface-200 dark:bg-surface-700 text-xs">
                      {lang}
                    </span>
                  {/each}
                </div>
              </div>
            {/if}
            <dl class="contact-view-grid">
              {#if selectedContact.timezone}
                <dt>{m.contact_form_label_timezone()}</dt><dd>{selectedContact.timezone}</dd>
              {/if}
            </dl>
            {#if selectedContact.note}
              <div class="contact-view-block">
                <span class="contact-view-block-label">{m.contact_form_label_notes()}</span>
                <p class="whitespace-pre-wrap text-sm">{selectedContact.note}</p>
              </div>
            {/if}
          </section>
        {/if}

        {#if !hasPersonalDetails(selectedContact)
          && !hasCommunicationDetails(selectedContact)
          && !hasWorkDetails(selectedContact)
          && !hasAddressWebDetails(selectedContact)
          && !hasEncryptionDetails(selectedContact)
          && !hasOtherDetails(selectedContact)}
          <p class="text-sm text-surface-500 italic">
            {m.contact_view_empty_state()}
          </p>
        {/if}
      </div>
    {:else}
      <!-- The `.contact-form` scope (see the style block below)
           flattens every `.input`-shaped control in this form —
           text inputs, textarea, and the `Select` / `DateField`
           triggers, which carry the same class — to a transparent
           outline-only fill and one slim SearchInput-height
           padding, so mixed rows (kind picker + input) stay even
           without per-control padding utilities having to agree. -->
      <div class="contact-form max-w-2xl w-full mx-auto p-6 flex flex-col gap-4">
        <!-- ── #143: avatar banner (click to upload) ─────────── -->
        <div class="flex items-center gap-4">
          <button
            type="button"
            onclick={() => void pickPhoto()}
            class="relative group w-20 h-20 rounded-full overflow-hidden bg-surface-300 dark:bg-surface-700 flex items-center justify-center text-2xl font-semibold cursor-pointer hover:ring-2 hover:ring-primary-500/50 transition"
            title={m.contact_form_avatar_title()}
            aria-label={m.contact_form_avatar_aria()}
          >
            {#if formAvatarSrc()}
              <img src={formAvatarSrc()} alt="" class="w-full h-full object-cover" />
            {:else}
              <span>{(formName || formGiven || '?').slice(0, 1).toUpperCase()}</span>
            {/if}
            <span class="absolute inset-0 flex items-center justify-center bg-black/40 text-white text-xs opacity-0 group-hover:opacity-100 transition">
              {m.contact_form_avatar_overlay()}
            </span>
          </button>
          <div class="flex flex-col flex-1 min-w-0">
            <h3 class="text-lg font-semibold truncate">
              {selectedId === 'new'
                ? m.contact_form_new_heading()
                : formName || `${formGiven} ${formFamily}`.trim() || m.contact_form_no_name()}
            </h3>
            {#if selectedContact}
              <span class="text-xs text-surface-500 truncate">
                {m.contact_form_from_account({ account: accountLabel(selectedContact.nextcloud_account_id) })}
              </span>
            {/if}
            {#if selectedPhotoBytes || formPhotoMime}
              <button
                type="button"
                class="text-xs text-error-500 hover:bg-red-500/20 rounded-lg px-2 py-1 self-start mt-1"
                onclick={() => {
                  selectedPhotoBytes = null
                  formPhotoMime = null
                  photoRemoved = true
                }}
              >{m.contact_form_remove_photo()}</button>
            {/if}
          </div>
        </div>

        <!-- ── Personal ──────────────────────────────────────── -->
        <details class="contact-form-section" open>
          <summary class="contact-form-section-title">{m.contact_form_section_personal()}</summary>
          <div class="contact-form-section-body">
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_prefix()}</span>
                <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formPrefix} placeholder="Dr." />
              </label>
              <label class="label">
                <span>{m.contact_form_label_suffix()}</span>
                <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formSuffix} placeholder="Jr." />
              </label>
            </div>
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_given_name()}</span>
                <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formGiven} placeholder="Jane" />
              </label>
              <label class="label">
                <span>{m.contact_form_label_family_name()}</span>
                <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formFamily} placeholder="Doe" />
              </label>
            </div>
            <label class="label">
              <span>{m.contact_form_label_additional_names()}</span>
              <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formAdditional} placeholder={m.contact_form_hint_additional_names()} />
            </label>
            <label class="label">
              <span>{m.contact_form_label_display_name()} <span class="text-surface-500">({m.contact_form_hint_display_name_auto()})</span></span>
              <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formName} placeholder="Jane Doe" />
            </label>
            <label class="label">
              <span>{m.contact_form_label_nickname()}</span>
              <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formNickname} placeholder="JD" />
            </label>
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_birthday()}</span>
                <DateField
                  ariaLabel={m.contact_form_label_birthday()}
                  bind:value={formBirthday}
                />
              </label>
              <label class="label">
                <span>{m.contact_form_label_anniversary()}</span>
                <DateField
                  ariaLabel={m.contact_form_label_anniversary()}
                  bind:value={formAnniversary}
                />
              </label>
            </div>
            <label class="label">
              <span>{m.contact_form_label_gender()}</span>
              <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formGender} placeholder={m.contact_form_placeholder_gender()} />
            </label>
          </div>
        </details>

        <!-- ── Communication ─────────────────────────────────── -->
        <details class="contact-form-section" open>
          <summary class="contact-form-section-title">{m.contact_form_section_communication()}</summary>
          <div class="contact-form-section-body">
            <div class="space-y-2">
              <span class="text-sm font-medium block">{m.contact_form_label_emails()}</span>
              {#each formEmails as email, i (i)}
                <div class="flex items-center gap-2">
                  <div class="w-28 shrink-0">
                    <Select bind:value={email.kind} options={emailKindOptions} />
                  </div>
                  <input
                    class="input flex-1 text-sm px-3 py-2 rounded-lg"
                    type="email"
                    bind:value={email.value}
                    placeholder="jane@example.com"
                  />
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                    aria-label={m.contact_form_button_remove()}
                    title={m.contact_form_button_remove()}
                    onclick={() => removeEmail(i)}
                  ><Icon name="trash" size={14} /></button>
                </div>
              {/each}
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 self-start inline-flex items-center justify-center"
                aria-label={m.contact_form_button_add_email()}
                title={m.contact_form_button_add_email()}
                onclick={addEmail}
              ><Icon name="plus" size={14} /></button>
            </div>

            <div class="space-y-2">
              <span class="text-sm font-medium block">{m.contact_form_label_phones()}</span>
              {#each formPhones as phone, i (i)}
                <div class="flex items-center gap-2">
                  <div class="w-28 shrink-0">
                    <Select bind:value={phone.kind} options={phoneKindOptions} />
                  </div>
                  <input
                    class="input flex-1 text-sm px-3 py-2 rounded-lg"
                    bind:value={phone.value}
                    placeholder="+1 555 0100"
                  />
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                    aria-label={m.contact_form_button_remove()}
                    title={m.contact_form_button_remove()}
                    onclick={() => removePhone(i)}
                  ><Icon name="trash" size={14} /></button>
                </div>
              {/each}
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 self-start inline-flex items-center justify-center"
                aria-label={m.contact_form_button_add_phone()}
                title={m.contact_form_button_add_phone()}
                onclick={addPhone}
              ><Icon name="plus" size={14} /></button>
            </div>

            <div class="space-y-2">
              <span class="text-sm font-medium block">{m.contact_form_label_impp()}</span>
              {#each formImpp as im, i (i)}
                <div class="flex items-center gap-2">
                  <div class="w-32 shrink-0">
                    <Select bind:value={im.kind} options={imppKindOptions} />
                  </div>
                  <input
                    class="input flex-1 text-sm px-3 py-2 rounded-lg"
                    bind:value={im.value}
                    placeholder={m.contact_form_placeholder_impp()}
                  />
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                    aria-label={m.contact_form_button_remove()}
                    title={m.contact_form_button_remove()}
                    onclick={() => removeImpp(i)}
                  ><Icon name="trash" size={14} /></button>
                </div>
              {/each}
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 self-start inline-flex items-center justify-center"
                aria-label={m.contact_form_button_add_impp()}
                title={m.contact_form_button_add_impp()}
                onclick={addImpp}
              ><Icon name="plus" size={14} /></button>
            </div>
          </div>
        </details>

        <!-- ── Work ─────────────────────────────────────────── -->
        <details class="contact-form-section" open>
          <summary class="contact-form-section-title">{m.contact_form_section_work()}</summary>
          <div class="contact-form-section-body">
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_organization()}</span>
                <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formOrg} placeholder="Example Corp" />
              </label>
              <label class="label">
                <span>{m.contact_form_label_job_title()}</span>
                <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formTitle} placeholder="Product Manager" />
              </label>
            </div>
            <label class="label">
              <span>{m.contact_form_label_role()} <span class="text-surface-500">({m.contact_form_hint_role()})</span></span>
              <input class="input text-sm px-3 py-2 rounded-lg" bind:value={formRole} placeholder="Project Lead" />
            </label>
            <div>
              <span class="text-sm font-medium mb-1 block">{m.contact_form_label_categories()}</span>
              <!-- Chips matching the read-only view's category
                   pills, with an inline X per chip to remove —
                   same pattern as the languages chip list below. -->
              {#if formCategories.length > 0}
                <div class="flex flex-wrap gap-1.5 mb-2">
                  {#each formCategories as cat, i (cat)}
                    <span class="inline-flex items-center gap-1 pl-2 pr-1 py-0.5 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300 text-xs">
                      {cat}
                      <button
                        type="button"
                        class="inline-flex items-center justify-center w-4 h-4 rounded-full hover:bg-primary-500/25"
                        aria-label={m.contact_form_button_remove()}
                        title={m.contact_form_button_remove()}
                        onclick={() => removeChip(formCategories, i, (next) => (formCategories = next))}
                      ><Icon name="close" size={10} /></button>
                    </span>
                  {/each}
                </div>
              {/if}
              <div class="relative">
                <input
                  class="input w-full text-sm px-3 py-2 rounded-lg"
                  bind:value={formCategoryDraft}
                  placeholder={m.contact_form_placeholder_chip()}
                  onfocus={() => (categoryFieldFocused = true)}
                  onblur={() => {
                    // Defer the close so a click on a suggestion fires
                    // before the input loses focus and removes it.
                    setTimeout(() => (categoryFieldFocused = false), 120)
                  }}
                  onkeydown={(e) => {
                    if (e.key === 'Enter' || e.key === ',') {
                      e.preventDefault()
                      commitChipDraft(formCategories, formCategoryDraft, (next, draft) => {
                        formCategories = next
                        formCategoryDraft = draft
                      })
                    } else if (e.key === 'Escape') {
                      categoryFieldFocused = false
                    }
                  }}
                />
                {#if categoryFieldFocused && filteredCategorySuggestions.length > 0}
                  <ul
                    class="absolute z-50 mt-1 w-full max-h-48 overflow-y-auto rounded-xl glass-float"
                    role="listbox"
                  >
                    {#each filteredCategorySuggestions as suggestion (suggestion)}
                      <li
                        role="option"
                        aria-selected="false"
                        class="px-3 py-1.5 text-sm cursor-pointer hover:bg-primary-500/15"
                        onmousedown={(e) => {
                          // mousedown beats blur — if we waited for click
                          // the input would have already closed the popover.
                          e.preventDefault()
                          formCategories = [...formCategories, suggestion]
                          formCategoryDraft = ''
                        }}
                      >{suggestion}</li>
                    {/each}
                  </ul>
                {/if}
              </div>
            </div>
          </div>
        </details>

        <!-- ── Address & Web ────────────────────────────────── -->
        <details class="contact-form-section" open>
          <summary class="contact-form-section-title">{m.contact_form_section_address_web()}</summary>
          <div class="contact-form-section-body">
            <div class="space-y-2">
              <span class="text-sm font-medium block">{m.contact_form_label_addresses()}</span>
              {#each formAddresses as addr, i (i)}
                <div class="card p-3 bg-surface-50 dark:bg-surface-900/50 rounded-lg space-y-2">
                  <div class="flex items-center gap-2">
                    <div class="w-32 shrink-0">
                      <Select bind:value={addr.kind} options={addressKindOptions} />
                    </div>
                    <button
                      type="button"
                      class="ml-auto text-error-500 hover:bg-red-500/20 rounded-lg p-1 inline-flex items-center justify-center"
                      aria-label={m.contact_form_button_remove()}
                      title={m.contact_form_button_remove()}
                      onclick={() => removeAddress(i)}
                    ><Icon name="trash" size={14} /></button>
                  </div>
                  <AddressSuggestField
                    street={addr.street}
                    enabled={geocodingEnabled}
                    placeholder={m.contact_form_placeholder_street()}
                    onstreetchange={(v) => {
                      addr.street = v
                    }}
                    onpick={(parts) => {
                      addr.street = parts.street
                      addr.locality = parts.locality
                      addr.region = parts.region
                      addr.postal_code = parts.postal_code
                      addr.country = parts.country
                    }}
                  />
                  <div class="grid grid-cols-2 gap-2">
                    <input class="input text-sm px-3 py-2 rounded-lg" bind:value={addr.locality} placeholder={m.contact_form_placeholder_city()} />
                    <input class="input text-sm px-3 py-2 rounded-lg" bind:value={addr.region} placeholder={m.contact_form_placeholder_region()} />
                  </div>
                  <div class="grid grid-cols-2 gap-2">
                    <input class="input text-sm px-3 py-2 rounded-lg" bind:value={addr.postal_code} placeholder={m.contact_form_placeholder_postal()} />
                    <input class="input text-sm px-3 py-2 rounded-lg" bind:value={addr.country} placeholder={m.contact_form_placeholder_country()} />
                  </div>
                </div>
              {/each}
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 self-start inline-flex items-center justify-center"
                aria-label={m.contact_form_button_add_address()}
                title={m.contact_form_button_add_address()}
                onclick={addAddress}
              ><Icon name="plus" size={14} /></button>
            </div>

            <div class="space-y-2">
              <span class="text-sm font-medium block">{m.contact_form_label_websites()}</span>
              {#each formWebsites as site, i (i)}
                <div class="flex items-center gap-2">
                  <input
                    class="input flex-1 text-sm px-3 py-2 rounded-lg"
                    type="url"
                    bind:value={site.value}
                    placeholder="https://example.com"
                  />
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                    aria-label={m.contact_form_button_remove()}
                    title={m.contact_form_button_remove()}
                    onclick={() => removeWebsite(i)}
                  ><Icon name="trash" size={14} /></button>
                </div>
              {/each}
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 self-start inline-flex items-center justify-center"
                aria-label={m.contact_form_button_add_website()}
                title={m.contact_form_button_add_website()}
                onclick={addWebsite}
              ><Icon name="plus" size={14} /></button>
            </div>
          </div>
        </details>

        <!-- ── Encryption (#57) ─────────────────────────────── -->
        <details class="contact-form-section" open>
          <summary class="contact-form-section-title">{m.contact_form_section_encryption()}</summary>
          <div class="contact-form-section-body">
            <div class="space-y-2">
              <span class="text-sm font-medium block">{m.contact_form_label_pgp_keys()}</span>
              {#each formKeys as key, i (i)}
                <div class="flex items-start gap-2">
                  <textarea
                    class="input flex-1 px-3 py-2 rounded-lg font-mono text-xs"
                    rows="4"
                    bind:value={key.value}
                    placeholder={m.contact_form_placeholder_pgp_key()}
                  ></textarea>
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                    aria-label={m.contact_form_button_remove()}
                    title={m.contact_form_button_remove()}
                    onclick={() => removeKey(i)}
                  ><Icon name="trash" size={14} /></button>
                </div>
              {/each}
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 self-start inline-flex items-center justify-center"
                aria-label={m.contact_form_button_add_pgp_key()}
                title={m.contact_form_button_add_pgp_key()}
                onclick={addKey}
              ><Icon name="plus" size={14} /></button>
              <p class="text-xs text-surface-400 leading-snug">
                {m.contact_form_hint_pgp_keys()}
              </p>
            </div>
          </div>
        </details>

        <!-- ── Other ────────────────────────────────────────── -->
        <details class="contact-form-section" open>
          <summary class="contact-form-section-title">{m.contact_form_section_other()}</summary>
          <div class="contact-form-section-body">
            <div>
              <span class="text-sm font-medium mb-1 block">{m.contact_form_label_languages()}</span>
              <div class="flex flex-wrap gap-1.5 mb-2">
                {#each formLanguages as lang, i (lang)}
                  <span class="inline-flex items-center gap-1 pl-2 pr-1 py-0.5 rounded-full bg-surface-200 dark:bg-surface-700 text-xs">
                    {lang}
                    <button
                      type="button"
                      class="inline-flex items-center justify-center w-4 h-4 rounded-full hover:bg-surface-400/30"
                      aria-label={m.contact_form_button_remove()}
                      title={m.contact_form_button_remove()}
                      onclick={() => removeChip(formLanguages, i, (next) => (formLanguages = next))}
                    ><Icon name="close" size={10} /></button>
                  </span>
                {/each}
              </div>
              <input
                class="input text-sm px-3 py-2 rounded-lg"
                bind:value={formLanguageDraft}
                placeholder={m.contact_form_placeholder_languages()}
                onkeydown={(e) => {
                  if (e.key === 'Enter' || e.key === ',') {
                    e.preventDefault()
                    commitChipDraft(formLanguages, formLanguageDraft, (next, draft) => {
                      formLanguages = next
                      formLanguageDraft = draft
                    })
                  }
                }}
                onblur={() => {
                  // Commit on blur too — a value typed without
                  // pressing Enter shouldn't look lost.  Save has
                  // its own draft fold-in as the safety net (#522).
                  commitChipDraft(formLanguages, formLanguageDraft, (next, draft) => {
                    formLanguages = next
                    formLanguageDraft = draft
                  })
                }}
              />
            </div>
            <label class="label">
              <span>{m.contact_form_label_timezone()}</span>
              <input
                class="input text-sm px-3 py-2 rounded-lg"
                bind:value={formTimezone}
                placeholder={m.contact_form_placeholder_timezone()}
              />
            </label>
            <label class="label">
              <span>{m.contact_form_label_notes()}</span>
              <textarea
                class="textarea text-sm px-3 py-2 rounded-lg"
                rows="3"
                bind:value={formNote}
                placeholder={m.contact_form_placeholder_notes()}
              ></textarea>
            </label>
          </div>
        </details>

        {#if selectedId === 'new'}
          <div class="grid grid-cols-2 gap-3">
            <label class="label">
              <span>Nextcloud account</span>
              <Select
                bind:value={formAccountId}
                options={accounts.map((a) => ({
                  value: a.id,
                  label: a.display_name ?? a.username,
                }))}
                onchange={() => onAccountChange()}
              />
            </label>
            <label class="label">
              <span>Addressbook</span>
              <Select
                value={formAddressbookUrl}
                options={(addressbooksByAccount[formAccountId] ?? []).map((b) => ({
                  value: b.path,
                  label: b.display_name ?? b.name,
                }))}
                onchange={(v) => onAddressbookChange(v)}
              />
            </label>
          </div>
        {/if}

        {#if formError}
          <p class="text-sm text-red-500">{formError}</p>
        {/if}

        <!-- Edit / create form action row.  Delete moved to the
             read-only view mode (top-right corner of that
             screen) so the editing path stays focused on
             "commit or discard the in-flight changes".  Icon-only
             confirm / cancel pair per the CLAUDE.md inline-form
             convention: `save-draft` for commit (swapping to
             `loading` mid-save), `close` for cancel. -->
        <div class="flex items-center gap-2 pt-2">
          <button
            class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
            disabled={saving}
            onclick={saveContact}
            title={saving ? m.contact_form_button_saving() : m.contact_form_button_save()}
            aria-label={saving ? m.contact_form_button_saving() : m.contact_form_button_save()}
          ><Icon name={saving ? 'loading' : 'save-draft'} size={14} /></button>
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
            disabled={saving}
            onclick={cancelEdit}
            title={m.contact_form_button_cancel()}
            aria-label={m.contact_form_button_cancel()}
          ><Icon name="close" size={14} /></button>
        </div>
      </div>
    {/if}
  </main>
  </div>
</div>

<!-- ── Sidebar row menus + emoji picker ─────────────────────────
     Rendered at the component root, OUTSIDE the glass sidebar:
     `.glass-panel`'s backdrop-filter makes the aside a containing
     block for position:fixed descendants, so a menu nested in a
     row would be offset by the aside's own viewport position
     instead of landing on the coordinates we measured (#522). -->
{#if openMenuFor && openMenuFor.startsWith('cat:')}
  {@const catMenuName = openMenuFor.slice('cat:'.length)}
  <div
    class="z-30 w-56 py-1 rounded-lg border border-surface-300 dark:border-surface-600 bg-surface-50 dark:bg-surface-900 shadow-lg text-sm"
    style="position: fixed; top: {menuTop}px; left: {menuLeft}px;"
    onclick={(e) => e.stopPropagation()}
    onmousedown={(e) => e.stopPropagation()}
    role="menu"
    tabindex="-1"
    onkeydown={(e) => { if (e.key === 'Escape') openMenuFor = null }}
  >
    <button
      class="w-full text-left px-3 py-2 hover:bg-primary-500/10"
      onclick={() => { openMenuFor = null; void renameCategory(catMenuName) }}
    >Rename…</button>
    <button
      class="w-full text-left px-3 py-2 hover:bg-error-500/10 text-error-500"
      onclick={() => { openMenuFor = null; void deleteCategory(catMenuName) }}
    >Delete</button>
  </div>
{/if}
{#if openMenuFor && openMenuFor.startsWith('ml:')}
  {@const menuListId = openMenuFor.slice('ml:'.length)}
  {@const menuList = mailingLists.find((l) => l.id === menuListId)}
  {#if menuList && menuList.source !== 'team'}
    <div
      class="z-30 w-56 py-1 rounded-lg border border-surface-300 dark:border-surface-600 bg-surface-50 dark:bg-surface-900 shadow-lg text-sm"
      style="position: fixed; top: {menuTop}px; left: {menuLeft}px;"
      onclick={(e) => e.stopPropagation()}
      onmousedown={(e) => e.stopPropagation()}
      role="menu"
      tabindex="-1"
      onkeydown={(e) => { if (e.key === 'Escape') openMenuFor = null }}
    >
      <button
        class="w-full text-left px-3 py-2 hover:bg-primary-500/10"
        onclick={() => { openMenuFor = null; startRenameMailingList(menuList) }}
      >Rename</button>
      <button
        class="w-full text-left px-3 py-2 hover:bg-primary-500/10"
        onclick={() => openEmojiPickerAtMenu(menuList)}
      >{menuList.emoji ? 'Change emoji' : 'Set emoji'}</button>
      {#if menuList.emoji}
        <button
          class="w-full text-left px-3 py-2 hover:bg-primary-500/10"
          onclick={() => { openMenuFor = null; void pickMailingListEmoji(menuList, null) }}
        >Remove emoji</button>
      {/if}
      {#if menuList.source === 'manual'}
        <button
          class="w-full text-left px-3 py-2 hover:bg-error-500/10 text-error-500"
          onclick={() => { openMenuFor = null; void deleteManualMailingList(menuList.id, menuList.name) }}
        >Delete</button>
      {/if}
    </div>
  {/if}
{/if}
{#if emojiPickerFor}
  {@const pickerList = mailingLists.find((l) => l.id === emojiPickerFor)}
  {#if pickerList}
    <div
      class="z-40"
      style="position: fixed; top: {emojiPickerTop}px; left: {emojiPickerLeft}px;"
      role="menu"
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
      onmousedown={(e) => e.stopPropagation()}
      onkeydown={(e) => { if (e.key === 'Escape') emojiPickerFor = null }}
    >
      <EmojiPicker
        value={pickerList.emoji}
        onpick={(emoji) => void pickMailingListEmoji(pickerList, emoji)}
      />
    </div>
  {/if}
{/if}

<!-- New mailing list modal — name + emoji avatar.  Mirrors the
     calendar create modal's shape so the UX feels consistent. -->
{#if newListForm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) newListForm = null }}
  >
    <div class="glass-float rounded-2xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-3">New mailing list</h3>

      <label class="block text-xs text-surface-500 mb-1" for="new-ml-name">Name</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="new-ml-name"
        type="text"
        class="input w-full text-sm px-2 py-1.5 rounded-lg mb-3"
        placeholder="Family, Team, Newsletter, …"
        bind:value={newListForm.name}
        disabled={newListBusy}
        autofocus
        onkeydown={(e) => {
          if (e.key === 'Enter' && newListForm?.name.trim()) {
            e.preventDefault()
            void commitNewMailingList()
          } else if (e.key === 'Escape') {
            e.preventDefault()
            newListForm = null
          }
        }}
      />

      <div class="text-xs text-surface-500 mb-1">Emoji (optional)</div>
      <div class="mb-4">
        <EmojiPicker
          value={newListForm.emoji}
          widthClass="w-full"
          onpick={(emoji) => { newListForm!.emoji = emoji }}
        />
      </div>

      {#if newListError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{newListError}</p>
      {/if}

      <!-- Icon-only confirm / cancel — same pair AND order as the
           inline-form convention: commit (`save-draft`, with the
           `loading` swap) on the left, cancel (`close`) on the
           right, tooltips + aria carrying the labels. -->
      <div class="flex justify-end gap-2">
        <button
          class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
          disabled={newListBusy || !newListForm.name.trim()}
          onclick={() => void commitNewMailingList()}
          title={newListBusy ? m.contacts_view_list_creating() : m.contacts_view_list_create()}
          aria-label={newListBusy ? m.contacts_view_list_creating() : m.contacts_view_list_create()}
        ><Icon name={newListBusy ? 'loading' : 'save-draft'} size={14} /></button>
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
          disabled={newListBusy}
          onclick={() => (newListForm = null)}
          title={m.contacts_view_list_cancel()}
          aria-label={m.contacts_view_list_cancel()}
        ><Icon name="close" size={14} /></button>
      </div>
    </div>
  </div>
{/if}

<!-- Import-contacts modal (#484) — pick a file + target addressbook,
     run the import, then show the report in place so the user sees
     what was skipped and why before closing. -->
{#if importForm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget && !importBusy) importForm = null }}
  >
    <div class="glass-float rounded-2xl w-md max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">{m.contact_import_title()}</h3>
      <p class="text-xs text-on-glass-muted mb-4">{m.contact_import_intro()}</p>

      <div class="grid grid-cols-2 gap-3 mb-3">
        <label class="label">
          <span>{m.contact_import_label_account()}</span>
          <Select
            value={importForm.accountId}
            options={accounts.map((a) => ({
              value: a.id,
              label: a.display_name ?? a.username,
            }))}
            onchange={(v) => void setImportAccount(v)}
          />
        </label>
        <label class="label">
          <span>{m.contact_import_label_addressbook()}</span>
          <Select
            value={importForm.addressbookUrl}
            options={(addressbooksByAccount[importForm.accountId] ?? []).map((b) => ({
              value: b.path,
              label: b.display_name ?? b.name,
            }))}
            onchange={(v) => onImportAddressbookChange(v)}
          />
        </label>
      </div>

      <div class="text-xs text-surface-500 mb-1">{m.contact_import_label_file()}</div>
      <div class="flex items-center gap-2 mb-4">
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5"
          disabled={importBusy}
          onclick={() => void pickImportFile()}
        >
          <Icon name="attachment" size={14} />
          {m.contact_import_choose_file()}
        </button>
        <span class="text-sm truncate min-w-0 {importForm.path ? '' : 'text-on-glass-muted'}">
          {importForm.path ? importFileName(importForm.path) : m.contact_import_no_file()}
        </span>
      </div>

      {#if importReport}
        <div class="mb-3">
          <div
            class="inline-flex items-center gap-1 text-xs text-success-500"
            aria-live="polite"
          >
            <Icon name="success" size={14} />
            <span>{m.contact_import_summary({
              imported: importReport.imported,
              total: importReport.total,
              skipped: importReport.skipped_duplicates,
            })}</span>
          </div>
          {#if importReport.errors.length > 0}
            <p class="text-xs text-surface-500 mt-2 mb-1">{m.contact_import_errors_label()}</p>
            <ul class="text-xs text-red-500 max-h-32 overflow-y-auto space-y-0.5">
              {#each importReport.errors as err}
                <li class="wrap-break-word">{err}</li>
              {/each}
            </ul>
          {/if}
        </div>
      {/if}

      {#if importError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{importError}</p>
      {/if}

      <div class="flex justify-end gap-2">
        {#if importReport}
          <button
            class="btn preset-filled-primary-500"
            onclick={() => (importForm = null)}
          >{m.contact_import_close()}</button>
        {:else}
          <button
            class="btn preset-outlined-surface-500"
            disabled={importBusy}
            onclick={() => (importForm = null)}
          >{m.contact_import_cancel()}</button>
          <button
            class="btn preset-filled-primary-500 inline-flex items-center gap-1.5"
            disabled={importBusy || !importForm.path || !importForm.addressbookUrl}
            onclick={() => void runImport()}
          >
            {#if importBusy}<Icon name="loading" size={14} />{/if}
            {importBusy ? m.contact_import_running() : m.contact_import_run()}
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<style>
  /* Compact field labels (#522) — Skeleton's `.label` span default
     reads one size too large next to the text-sm inputs; align
     with the settings panels' small muted label voice instead. */
  :global(.contact-form .label > span) {
    font-size: 0.75rem;
    color: var(--color-surface-500);
  }

  /* Outline-only form controls (#522).  A long stack of wide
     filled boxes reads heavy on the plain canvas, so inside this
     form the central `.input` fill goes transparent — the border
     alone carries the "type here" signal, and the app-wide primary
     focus ring is untouched.  `padding-block` unifies every
     control (raw inputs, textarea, and the Select / DateField
     triggers that ship their own py-2) at SearchInput's slim
     py-1.5 height; being unlayered, these rules deliberately win
     over the per-control padding utilities. */
  :global(.contact-form .input),
  :global(.contact-form .textarea) {
    background-color: transparent;
    padding-block: 0.375rem;
  }

  /* #143 — flat collapsible section header.  No card chrome /
     background; just a clickable title with a chevron and an
     underline so groups are visually separated without the
     surrounding box clipping any popovers (DateField / Select)
     that grow outside the section. */
  :global(.contact-form-section) {
    display: block;
  }
  :global(.contact-form-section-title) {
    cursor: pointer;
    user-select: none;
    list-style: none;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-weight: 600;
    font-size: 0.875rem;
    padding-bottom: 0.375rem;
    margin: 0;
    border-bottom: 1px solid var(--color-surface-300);
  }
  :global([data-mode='dark'] .contact-form-section-title) {
    border-bottom-color: var(--color-surface-700);
  }
  :global(.contact-form-section-title::-webkit-details-marker) {
    display: none;
  }
  :global(.contact-form-section-title::before) {
    content: '▸';
    color: var(--color-surface-500);
    font-size: 0.75rem;
    transition: transform 120ms ease;
  }
  :global(.contact-form-section[open] > .contact-form-section-title::before) {
    transform: rotate(90deg);
  }
  :global(.contact-form-section-body) {
    display: flex;
    flex-direction: column;
    gap: 0.875rem;
    padding-top: 0.875rem;
  }

  /* #143 follow-up: read-only view-mode chrome.  No card
     borders here — the right pane is already a panel; we use
     just typography + spacing to separate sections.  Section
     titles get a thin underline so the eye picks out the
     boundaries without heavy chrome. */
  :global(.contact-view-section) {
    display: flex;
    flex-direction: column;
    gap: 0.625rem;
  }
  :global(.contact-view-section-title) {
    font-size: 0.75rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--color-surface-500);
    border-bottom: 1px solid var(--color-surface-200);
    padding-bottom: 0.25rem;
    margin: 0;
  }
  :global([data-mode='dark'] .contact-view-section-title) {
    border-bottom-color: var(--color-surface-700);
  }
  /* Two-column dl: label on the left, value on the right.
     Label width is just generous enough for the longer
     translations (Anniversary / Geburtstag / etc.) without
     pushing the value column too far. */
  :global(.contact-view-grid) {
    display: grid;
    grid-template-columns: minmax(8rem, max-content) 1fr;
    gap: 0.375rem 1rem;
    margin: 0;
  }
  :global(.contact-view-grid > dt) {
    font-size: 0.75rem;
    color: var(--color-surface-500);
    align-self: center;
  }
  :global(.contact-view-grid > dd) {
    font-size: 0.875rem;
    margin: 0;
  }
  /* For multi-row groupings (emails / phones / addresses /
     etc.) we use a labeled block instead of dl — the label
     sits above the rows and each row carries its own kind
     prefix. */
  :global(.contact-view-block) {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  :global(.contact-view-block-label) {
    font-size: 0.75rem;
    color: var(--color-surface-500);
  }
  :global(.contact-view-row) {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    font-size: 0.875rem;
  }
  :global(.contact-view-row-kind) {
    font-size: 0.75rem;
    color: var(--color-surface-500);
    min-width: 4.5rem;
  }
  :global(.contact-view-row-value) {
    flex: 1;
    min-width: 0;
  }
  :global(.contact-view-address) {
    display: grid;
    grid-template-columns: 4.5rem 1fr;
    gap: 0.25rem 0.5rem;
    margin-bottom: 0.375rem;
    font-size: 0.875rem;
  }
  :global(.contact-view-address-lines > div) {
    line-height: 1.4;
  }
</style>
