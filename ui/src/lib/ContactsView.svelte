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

  import { convertFileSrc, invoke } from '@tauri-apps/api/core'
  import { open as openFileDialog } from '@tauri-apps/plugin-dialog'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'
  import EmojiPicker from './EmojiPicker.svelte'
  import Icon, { type IconName } from './Icon.svelte'

  interface Props {
    onclose: () => void
  }
  const { onclose }: Props = $props()

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
  /** Mirrors `nimbus_core::models::StructuredName`. */
  interface StructuredName {
    family: string
    given: string
    additional: string
    prefix: string
    suffix: string
  }
  /** Mirrors `nimbus_core::models::ContactImpp`. */
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
  }
  interface AddressbookSummary {
    path: string
    name: string
    display_name: string | null
  }

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<NextcloudAccount[]>([])
  let contacts = $state<Contact[]>([])
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
  let formGeo = $state('')
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
  let formUrls = $state('') // newline-separated
  // Addresses are an array of records, edited in place. We model a
  // single concatenated free-text field per address keeping
  // street/locality/region/postal/country on separate lines so the
  // form stays readable without exploding into one input per slot.
  let formAddresses = $state<ContactAddress[]>([])
  let formAccountId = $state('')       // only used for create
  let formAddressbookUrl = $state('')  // only used for create
  let formAddressbookName = $state('') // only used for create
  // Photo bytes for the selected contact, lazy-loaded via
  // `get_contact_photo`. Only fetched so we can round-trip them
  // through `update_contact` — display uses the `contact-photo://`
  // URI scheme, which streams bytes straight to `<img>` without
  // touching JSON IPC. Without this round-trip the vCard rebuild
  // on save would drop the avatar.
  let selectedPhotoBytes = $state<number[] | null>(null)
  let saving = $state(false)
  let formError = $state('')
  let deleteConfirm = $state(false)

  // Cache per-account addressbooks so switching the "save to" account
  // in the new-contact form doesn't re-hit the server.
  let addressbooksByAccount = $state<Record<string, AddressbookSummary[]>>({})

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
          const books = await invoke<AddressbookSummary[]>(
            'list_nextcloud_addressbooks',
            { ncId: a.id },
          )
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
      categories = await invoke<ContactCategoryView[]>('list_contact_categories')
    } catch (e) {
      console.warn('list_contact_categories failed', e)
    }
    try {
      mailingLists = await invoke<MailingListView[]>('list_mailing_lists')
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
        await invoke('add_contact_to_category', { contactId: id, category: name })
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
      await invoke('rename_contact_category', { old: name, new: next })
      await reloadContacts()
      if (selectedScope === `category:${name}`) selectedScope = `category:${next}`
    } catch (e) {
      formError = formatError(e) || 'Failed to rename Contact Group'
    }
  }
  async function deleteCategory(name: string) {
    if (!confirm(`Remove the "${name}" tag from every contact carrying it? Contacts themselves are kept.`)) return
    try {
      await invoke('delete_contact_category', { name })
      await reloadContacts()
      if (selectedScope === `category:${name}`) selectedScope = 'all'
    } catch (e) {
      formError = formatError(e) || 'Failed to delete Contact Group'
    }
  }
  async function toggleCategoryAsList(name: string, currentlyOn: boolean) {
    try {
      await invoke('set_category_use_as_mailing_list', {
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
      await invoke('add_contact_to_category', { contactId, category: name })
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
        books = await invoke<AddressbookSummary[]>(
          'list_nextcloud_addressbooks',
          { ncId },
        )
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
      const created = await invoke<{ id: string }>('create_contact_group', {
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
          await invoke('set_mailing_list_emoji', {
            id: `list:${created.id}`,
            emoji: chosenEmoji,
          })
        } catch (e) {
          console.warn('set_mailing_list_emoji failed', e)
        }
      }
      try {
        mailingLists = await invoke<MailingListView[]>('list_mailing_lists')
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
      await invoke('delete_contact_group', { groupId })
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
        await invoke('update_contact_group', {
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
      contacts = contacts.map((c) => {
        if (c.id !== contactId) return c
        const cats = c.categories ?? []
        return cats.includes(ml.name) ? c : { ...c, categories: [...cats, ml.name] }
      })
      try {
        await invoke('add_contact_to_category', { contactId, category: ml.name })
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
        await invoke('update_contact_group', {
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
      contacts = contacts.map((c) =>
        c.id === target.id
          ? { ...c, categories: (c.categories ?? []).filter((cat) => cat !== ml.name) }
          : c,
      )
      try {
        await invoke('remove_contact_from_category', {
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
      await invoke('set_mailing_list_hidden', { id, hidden: !currently })
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
      await invoke('rename_mailing_list', { id: ml.id, newName: next })
      // Category renames change the row's id (cat:<old> →
      // cat:<new>); refetch picks up the new id while keeping
      // per-row settings in sync.  Manual lists keep their id.
      if (ml.source === 'category') {
        if (selectedListId === ml.id) selectedListId = `cat:${next}`
        mailingLists = await invoke<MailingListView[]>('list_mailing_lists')
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

  function openEmojiPickerFor(ml: MailingListView, anchor: HTMLElement) {
    const r = anchor.getBoundingClientRect()
    emojiPickerTop = r.bottom + 4
    emojiPickerLeft = r.left
    emojiPickerFor = ml.id
  }

  async function pickMailingListEmoji(ml: MailingListView, emoji: string | null) {
    emojiPickerFor = null
    try {
      await invoke('set_mailing_list_emoji', { id: ml.id, emoji })
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
      accounts = await invoke<NextcloudAccount[]>('get_nextcloud_accounts')
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
    contacts = await invoke<Contact[]>('get_contacts', { ncId: null })
    contacts.sort((a, b) =>
      a.display_name.localeCompare(b.display_name, undefined, { sensitivity: 'base' }),
    )
    await loadSidebarData()
  }

  async function syncInBackground() {
    if (syncing) return
    syncing = true
    try {
      for (const a of accounts) {
        try {
          await invoke('sync_nextcloud_contacts', { ncId: a.id })
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
      const books = await invoke<AddressbookSummary[]>(
        'list_nextcloud_addressbooks',
        { ncId },
      )
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

  function selectContact(id: string) {
    selectedId = id
    deleteConfirm = false
    formError = ''
    const c = contacts.find((x) => x.id === id)
    if (!c) return
    formName = c.display_name
    formEmails = c.email.map((e) => ({ ...e }))
    formPhones = c.phone.map((p) => ({ ...p }))
    formOrg = c.organization ?? ''
    formTitle = c.title ?? ''
    formBirthday = c.birthday ?? ''
    formNote = c.note ?? ''
    formUrls = (c.urls ?? []).join('\n')
    formAddresses = (c.addresses ?? []).map((a) => ({ ...a }))
    selectedPhotoBytes = null
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
    formGeo = c.geo ?? ''
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
    formName = ''
    formEmails = []
    formPhones = []
    formOrg = ''
    formTitle = ''
    formBirthday = ''
    formNote = ''
    formUrls = ''
    formAddresses = []
    selectedPhotoBytes = null
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
    formGeo = ''
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
    selectedId = null
    formError = ''
    deleteConfirm = false
    selectedPhotoBytes = null
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
    if (selectedId === null) return
    if (saving) return
    if (document.querySelector('[role="listbox"]')) return
    e.preventDefault()
    cancelEdit()
  }

  // Pull just the bytes via IPC so we can round-trip them on save.
  // Display elsewhere uses `photoSrc()` against the URI scheme.
  async function loadSelectedPhotoBytes(id: string) {
    try {
      const photo = await invoke<{ mime: string; data: number[] } | null>(
        'get_contact_photo',
        { contactId: id },
      )
      if (selectedId !== id) return
      selectedPhotoBytes = photo?.data ?? null
    } catch (e) {
      console.warn('get_contact_photo failed', e)
    }
  }

  // URL for `<img src>` against the custom Tauri URI scheme. Bytes
  // are streamed straight from the cache to the webview — no JSON
  // bloat, browser handles caching, `loading="lazy"` defers off-
  // screen rows. Returns `null` when the contact has no photo so
  // callers can render the initial-letter placeholder instead.
  function photoSrc(c: Contact): string | null {
    if (!c.photo_mime) return null
    return convertFileSrc(c.id, 'contact-photo')
  }

  /** Lookup map keyed by lowercase email so the mailing-list
   *  member rows (which only carry `{displayName, email}`)
   *  can resolve a matching Contact for its photo (#179). */
  const contactByEmail = $derived.by(() => {
    const m = new Map<string, Contact>()
    for (const c of contacts) {
      for (const e of c.email) {
        if (e.value) m.set(e.value.toLowerCase(), c)
      }
    }
    return m
  })

  function onAccountChange() {
    void loadAddressbooksFor(formAccountId)
  }

  function onAddressbookChange(e: Event) {
    const sel = e.target as HTMLSelectElement
    const books = addressbooksByAccount[formAccountId] ?? []
    const picked = books.find((b) => b.path === sel.value)
    formAddressbookUrl = sel.value
    formAddressbookName = picked?.name ?? ''
  }

  // Split a textarea's contents into trimmed non-empty lines. vCard
  // emits one EMAIL / TEL per value, so the form's newline separation
  // maps 1:1 onto the backend shape.
  function splitLines(s: string): string[] {
    return s
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.length > 0)
  }

  function buildInput(): ContactInput {
    // The form now owns the photo MIME via `formPhotoMime`, which
    // is loaded from the cached contact on open (#143) and
    // overwritten by the click-to-upload pipeline.  Combined with
    // `selectedPhotoBytes`, that gives us a complete photo
    // payload regardless of whether the avatar came from the
    // server or from a fresh upload.
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
      photo_mime: formPhotoMime,
      photo_data: formPhotoMime ? selectedPhotoBytes : null,
      title: formTitle.trim() || null,
      birthday: formBirthday.trim() || null,
      note: formNote.trim() || null,
      urls: splitLines(formUrls),
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
      nickname: formNickname.trim() || null,
      anniversary: formAnniversary.trim() || null,
      gender: formGender.trim() || null,
      role: formRole.trim() || null,
      geo: formGeo.trim() || null,
      timezone: formTimezone.trim() || null,
      impp: formImpp
        .filter((i) => i.value.trim())
        .map((i) => ({ kind: i.kind, value: i.value.trim() })),
      languages: [...formLanguages],
      categories: [...formCategories],
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
      const picked = await openFileDialog({
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
      const url = convertFileSrc(picked)
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
    } catch (e) {
      console.warn('pickPhoto failed', e)
    }
  }

  /** Data URL for the freshly-picked photo, used by the form's
   *  avatar `<img>` so the user sees the new picture before
   *  save.  Falls back to the existing photoSrc when no fresh
   *  upload has happened yet. */
  function formAvatarSrc(): string | null {
    if (selectedPhotoBytes && formPhotoMime) {
      // Build a data URL from the in-memory bytes.  Small enough
      // for typical avatars (a few hundred KB) that the base64
      // round-trip cost is negligible.
      const bin = new Uint8Array(selectedPhotoBytes)
      let s = ''
      for (let i = 0; i < bin.length; i++) s += String.fromCharCode(bin[i])
      return `data:${formPhotoMime};base64,${btoa(s)}`
    }
    if (selectedContact && photoSrc(selectedContact)) return photoSrc(selectedContact)
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
  function addImpp() {
    formImpp = [...formImpp, { kind: 'matrix', value: '' }]
  }
  function removeImpp(idx: number) {
    formImpp = formImpp.filter((_, i) => i !== idx)
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
        const created = await invoke<Contact>('create_contact', {
          ncId: formAccountId,
          addressbookUrl: formAddressbookUrl,
          addressbookName: formAddressbookName,
          input,
        })
        await reloadContacts()
        selectedId = created.id
      } else if (selectedId) {
        const updated = await invoke<Contact>('update_contact', {
          contactId: selectedId,
          input,
        })
        await reloadContacts()
        selectedId = updated.id
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
      await invoke('delete_contact', { contactId: selectedId })
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

<div class="h-full flex bg-surface-50 dark:bg-surface-900">
  <!-- ── Sidebar — Contacts heading, tab strip, and (when on
       the Contacts tab) addressbooks + Contact Groups.  The
       sidebar stays mounted on the Mailing lists tab so the
       tab strip + heading don't move; only the navigation
       sections collapse. ───────────────────────────────────── -->
  <aside class="w-56 shrink-0 border-r border-surface-200 dark:border-surface-700 bg-surface-100/60 dark:bg-surface-800/40 flex flex-col">
    <div class="p-3 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
      <button
        class="btn-icon btn-icon-sm preset-tonal"
        aria-label="Back"
        onclick={onclose}
      >&larr;</button>
      <h2 class="text-base font-semibold flex-1 truncate">
        {activeTab === 'contacts' ? 'Contacts' : 'Mailing lists'}
      </h2>
      {#if syncing}
        <span class="text-[10px] text-surface-500">Syncing…</span>
      {/if}
    </div>
    <!-- Tab strip.  Buttons explicitly stop propagation +
         set activeTab on a separate handler so any pending
         document-level click-outside listener can't race the
         state update on the first transition. -->
    <div class="px-3 pt-2 flex gap-1">
      <button
        type="button"
        class="flex-1 px-2 py-2 text-sm rounded-md transition-colors {activeTab === 'contacts'
          ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
          : 'text-surface-600 dark:text-surface-300 hover:bg-surface-200 dark:hover:bg-surface-700'}"
        onclick={() => selectTab('contacts')}
      >Contacts</button>
      <button
        type="button"
        class="flex-1 px-2 py-2 text-sm rounded-md transition-colors {activeTab === 'lists'
          ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
          : 'text-surface-600 dark:text-surface-300 hover:bg-surface-200 dark:hover:bg-surface-700'}"
        onclick={() => selectTab('lists')}
      >Lists</button>
    </div>
    {#if activeTab === 'contacts'}
    <div class="flex-1 overflow-y-auto px-2 py-3 space-y-1">
      <!-- "All" -->
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm text-left transition-colors {selectedScope === 'all'
          ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
          : 'hover:bg-surface-200 dark:hover:bg-surface-700'}"
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
            class="w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm text-left transition-colors
                   {sel
                     ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
                     : 'hover:bg-surface-200 dark:hover:bg-surface-700'}"
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
          class="w-5 h-5 rounded-md flex items-center justify-center text-surface-500 hover:bg-surface-200 dark:hover:bg-surface-700"
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
          class="w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm text-left transition-colors cursor-pointer
                 {sel
                   ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
                   : 'hover:bg-surface-200 dark:hover:bg-surface-700'}
                 {dragOver ? 'ring-2 ring-primary-500' : ''}"
          oncontextmenu={(e) => {
            e.preventDefault()
            const action = prompt(
              `"${c.name}" — type: rename / delete`,
              '',
            )?.trim()
            if (action === 'rename') void renameCategory(c.name)
            else if (action === 'delete') void deleteCategory(c.name)
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
          <!-- Three-dot menu: rename, delete, toggle "use as
               mailing list".  Replaces the previous inline
               swatch — clearer for first-time users since the
               actions read as labelled words rather than a
               coloured square. -->
          <div class="relative shrink-0">
            <button
              class="w-5 h-5 rounded text-surface-500 hover:bg-surface-300 dark:hover:bg-surface-600 leading-none"
              title="More actions"
              aria-label="Contact Group actions"
              onclick={(e) => {
                e.stopPropagation()
                const r = (e.currentTarget as HTMLElement).getBoundingClientRect()
                menuTop = r.top
                menuLeft = r.right + 6
                openMenuFor = openMenuFor === `cat:${c.name}` ? null : `cat:${c.name}`
              }}
            >⋯</button>
            {#if openMenuFor === `cat:${c.name}`}
              <div
                class="z-30 w-56 py-1 rounded-md border border-surface-300 dark:border-surface-600 bg-surface-50 dark:bg-surface-900 shadow-lg text-sm"
                style="position: fixed; top: {menuTop}px; left: {menuLeft}px;"
                onclick={(e) => e.stopPropagation()}
                role="menu"
                tabindex="-1"
                onkeydown={(e) => { if (e.key === 'Escape') openMenuFor = null }}
              >
                <button
                  class="w-full text-left px-3 py-2 hover:bg-surface-200 dark:hover:bg-surface-700"
                  onclick={() => { openMenuFor = null; void renameCategory(c.name) }}
                >Rename…</button>
                <button
                  class="w-full text-left px-3 py-2 hover:bg-error-500/10 text-error-500"
                  onclick={() => { openMenuFor = null; void deleteCategory(c.name) }}
                >Delete</button>
              </div>
            {/if}
          </div>
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
            class="w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm cursor-pointer transition-colors {sel
              ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
              : 'hover:bg-surface-200 dark:hover:bg-surface-700'}"
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
              menuTop = e.clientY
              menuLeft = e.clientX
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
                  const r = (e.currentTarget as HTMLElement).getBoundingClientRect()
                  menuTop = r.top
                  menuLeft = r.right + 6
                  openMenuFor = openMenuFor === `ml:${ml.id}` ? null : `ml:${ml.id}`
                }}
              >⋯</button>
            {/if}
          </div>
          {#if openMenuFor === `ml:${ml.id}` && ml.source !== 'team'}
            <div
              class="z-30 w-56 py-1 rounded-md border border-surface-300 dark:border-surface-600 bg-surface-50 dark:bg-surface-900 shadow-lg text-sm"
              style="position: fixed; top: {menuTop}px; left: {menuLeft}px;"
              onclick={(e) => e.stopPropagation()}
              onmousedown={(e) => e.stopPropagation()}
              role="menu"
              tabindex="-1"
              onkeydown={(e) => { if (e.key === 'Escape') openMenuFor = null }}
            >
              <button
                class="w-full text-left px-3 py-2 hover:bg-surface-200 dark:hover:bg-surface-700"
                onclick={() => { openMenuFor = null; startRenameMailingList(ml) }}
              >Rename</button>
              <button
                class="w-full text-left px-3 py-2 hover:bg-surface-200 dark:hover:bg-surface-700"
                onclick={(e) => {
                  // Anchor the picker to the row, not the menu
                  // item — the menu closes immediately so its
                  // bounding rect is gone by the time we render.
                  const row = (e.currentTarget as HTMLElement).closest('.relative') as HTMLElement | null
                  openMenuFor = null
                  if (row) openEmojiPickerFor(ml, row)
                }}
              >{ml.emoji ? 'Change emoji' : 'Set emoji'}</button>
              {#if ml.emoji}
                <button
                  class="w-full text-left px-3 py-2 hover:bg-surface-200 dark:hover:bg-surface-700"
                  onclick={() => { openMenuFor = null; void pickMailingListEmoji(ml, null) }}
                >Remove emoji</button>
              {/if}
              {#if ml.source === 'manual'}
                <button
                  class="w-full text-left px-3 py-2 hover:bg-error-500/10 text-error-500"
                  onclick={() => { openMenuFor = null; void deleteManualMailingList(ml.id, ml.name) }}
                >Delete</button>
              {/if}
            </div>
          {/if}
          {#if emojiPickerFor === ml.id}
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
                value={ml.emoji}
                onpick={(emoji) => void pickMailingListEmoji(ml, emoji)}
              />
            </div>
          {/if}
        </div>
      {/snippet}

      <!-- Mailing lists — the manual KIND:group cards.  Header
           gets its own `+` so creating one feels symmetric with
           the Contact Groups section in the Contacts tab. -->
      <div class="px-3 pt-1 pb-1 flex items-center justify-between">
        <span class="text-[10px] uppercase tracking-wider text-surface-500">Mailing lists</span>
        <button
          class="w-5 h-5 rounded-md flex items-center justify-center text-surface-500 hover:bg-surface-200 dark:hover:bg-surface-700"
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
  <aside class="w-80 shrink-0 border-r border-surface-200 dark:border-surface-700 bg-surface-100 dark:bg-surface-800 flex flex-col">
    {#if activeTab === 'contacts'}
    <div class="p-3 flex flex-col gap-2">
      <div class="relative">
        <span class="absolute left-2 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
          <Icon name="search" size={14} />
        </span>
        <input
          type="search"
          class="input pl-7"
          placeholder="Search"
          bind:value={query}
        />
      </div>
      <button class="btn preset-filled-primary-500" onclick={startNew}>
        + New contact
      </button>
    </div>

    <div class="flex-1 overflow-y-auto px-2 pb-3">
      {#if loading}
        <p class="px-3 py-2 text-xs text-surface-500">Loading contacts…</p>
      {:else if error}
        <p class="px-3 py-2 text-xs text-red-500">{error}</p>
      {:else if contacts.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500">
          No contacts yet. Click “New contact” to add one.
        </p>
      {:else if filteredContacts.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500">No matches for “{query}”.</p>
      {:else}
        {#each filteredContacts as c (c.id)}
          <button
            class="w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm transition-colors
              {selectedId === c.id
                ? 'bg-primary-500/10 text-primary-500 font-medium'
                : 'hover:bg-surface-200 dark:hover:bg-surface-700'}"
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
            {#if photoSrc(c)}
              <img
                src={photoSrc(c)}
                alt=""
                loading="lazy"
                class="w-8 h-8 rounded-full object-cover shrink-0"
              />
            {:else}
              <span class="w-8 h-8 rounded-full bg-surface-300 dark:bg-surface-700 text-xs font-semibold flex items-center justify-center shrink-0">
                {c.display_name.slice(0, 1).toUpperCase()}
              </span>
            {/if}
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
        <div class="p-3 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
          <h3 class="text-base font-semibold flex-1 truncate">{ml.name}</h3>
          <span class="text-xs text-surface-500">
            {ml.members.filter((m) => m.email).length} / {ml.members.length}
          </span>
        </div>
        <!-- Search + Add Contact row.  Search filters the
             member list inline.  + Add Contact flips the
             column into a contact-picker that lists every
             contact NOT already in the list. -->
        <!-- Search field + Add Contact button stack — same
             vertical layout the Contacts tab middle column
             uses (search on top, primary action below) for
             visual consistency. -->
        <div class="p-3 flex flex-col gap-2 border-b border-surface-200 dark:border-surface-700">
          {#if pickerOpen}
            <input
              type="search"
              class="input"
              placeholder="Search contacts to add"
              bind:value={pickerQuery}
            />
          {:else}
            <input
              type="search"
              class="input"
              placeholder="Search members"
              bind:value={memberQuery}
            />
          {/if}
          {#if editable}
            <button
              class="btn preset-filled-primary-500"
              onclick={() => {
                pickerOpen = !pickerOpen
                pickerQuery = ''
              }}
            >{pickerOpen ? 'Done' : '+ Add contact'}</button>
          {/if}
        </div>
        <div class="flex-1 overflow-y-auto p-3 space-y-1">
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
                class="w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm text-left hover:bg-surface-200 dark:hover:bg-surface-700"
                onclick={() => void addContactToSelectedList(c.id)}
              >
                <span class="w-7 h-7 rounded-full bg-surface-300 dark:bg-surface-600 text-xs font-semibold flex items-center justify-center shrink-0">
                  {c.display_name.slice(0, 1).toUpperCase()}
                </span>
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
              {@const linkedContact = m.email ? contactByEmail.get(m.email.toLowerCase()) : undefined}
              {@const memberPhoto = linkedContact ? photoSrc(linkedContact) : null}
              <div class="group flex items-center gap-2 px-3 py-2 rounded-md bg-surface-200/40 dark:bg-surface-700/40">
                {#if memberPhoto}
                  <img
                    src={memberPhoto}
                    alt=""
                    loading="lazy"
                    class="w-7 h-7 rounded-full object-cover shrink-0"
                  />
                {:else}
                  <span class="w-7 h-7 rounded-full bg-surface-300 dark:bg-surface-600 text-xs font-semibold flex items-center justify-center shrink-0">
                    {(m.displayName || m.email || '?').slice(0, 1).toUpperCase()}
                  </span>
                {/if}
                <div class="flex-1 min-w-0">
                  <p class="font-medium truncate">{m.displayName || m.email || '(unnamed)'}</p>
                  <p class="text-xs text-surface-500 truncate">
                    {m.email || '(no email)'}
                  </p>
                </div>
                {#if editable && m.email}
                  <button
                    class="opacity-0 group-hover:opacity-100 transition-opacity w-7 h-7 rounded-md text-surface-500 hover:bg-error-500/15 hover:text-error-500 leading-none shrink-0"
                    title="Remove from list"
                    aria-label="Remove from list"
                    onclick={() => void removeContactFromSelectedList(m.email)}
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
        Pick a contact on the left, or click “New contact”.
      </div>
    {:else}
      <div class="max-w-2xl w-full mx-auto p-6 flex flex-col gap-4">
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
            {#if selectedPhotoBytes}
              <button
                type="button"
                class="text-xs text-error-500 hover:underline self-start mt-1"
                onclick={() => {
                  selectedPhotoBytes = null
                  formPhotoMime = null
                }}
              >{m.contact_form_remove_photo()}</button>
            {/if}
          </div>
        </div>

        <!-- ── Personal ──────────────────────────────────────── -->
        <details class="contact-section" open>
          <summary class="contact-section-summary">{m.contact_form_section_personal()}</summary>
          <div class="contact-section-body">
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_prefix()}</span>
                <input class="input" bind:value={formPrefix} placeholder="Dr." />
              </label>
              <label class="label">
                <span>{m.contact_form_label_suffix()}</span>
                <input class="input" bind:value={formSuffix} placeholder="Jr." />
              </label>
            </div>
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_given_name()}</span>
                <input class="input" bind:value={formGiven} placeholder="Jane" />
              </label>
              <label class="label">
                <span>{m.contact_form_label_family_name()}</span>
                <input class="input" bind:value={formFamily} placeholder="Doe" />
              </label>
            </div>
            <label class="label">
              <span>{m.contact_form_label_additional_names()}</span>
              <input class="input" bind:value={formAdditional} placeholder={m.contact_form_hint_additional_names()} />
            </label>
            <label class="label">
              <span>{m.contact_form_label_display_name()} <span class="text-surface-500">({m.contact_form_hint_display_name_auto()})</span></span>
              <input class="input" bind:value={formName} placeholder="Jane Doe" />
            </label>
            <label class="label">
              <span>{m.contact_form_label_nickname()}</span>
              <input class="input" bind:value={formNickname} placeholder="JD" />
            </label>
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_birthday()}</span>
                <input class="input" bind:value={formBirthday} placeholder="1985-10-31" />
              </label>
              <label class="label">
                <span>{m.contact_form_label_anniversary()}</span>
                <input class="input" bind:value={formAnniversary} placeholder="2018-06-15" />
              </label>
            </div>
            <label class="label">
              <span>{m.contact_form_label_gender()}</span>
              <input class="input" bind:value={formGender} placeholder={m.contact_form_placeholder_gender()} />
            </label>
          </div>
        </details>

        <!-- ── Communication ─────────────────────────────────── -->
        <details class="contact-section" open>
          <summary class="contact-section-summary">{m.contact_form_section_communication()}</summary>
          <div class="contact-section-body">
            <div class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium">{m.contact_form_label_emails()}</span>
                <button type="button" class="btn btn-sm preset-tonal" onclick={addEmail}>
                  {m.contact_form_button_add_email()}
                </button>
              </div>
              {#each formEmails as email, i (i)}
                <div class="flex items-center gap-2">
                  <select class="select w-28" bind:value={email.kind}>
                    <option value="home">{m.contact_form_kind_home()}</option>
                    <option value="work">{m.contact_form_kind_work()}</option>
                    <option value="other">{m.contact_form_kind_other()}</option>
                  </select>
                  <input
                    class="input flex-1"
                    type="email"
                    bind:value={email.value}
                    placeholder="jane@example.com"
                  />
                  <button
                    type="button"
                    class="text-xs text-error-500 hover:underline"
                    onclick={() => removeEmail(i)}
                  >{m.contact_form_button_remove()}</button>
                </div>
              {/each}
            </div>

            <div class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium">{m.contact_form_label_phones()}</span>
                <button type="button" class="btn btn-sm preset-tonal" onclick={addPhone}>
                  {m.contact_form_button_add_phone()}
                </button>
              </div>
              {#each formPhones as phone, i (i)}
                <div class="flex items-center gap-2">
                  <select class="select w-28" bind:value={phone.kind}>
                    <option value="cell">{m.contact_form_kind_mobile()}</option>
                    <option value="work">{m.contact_form_kind_work()}</option>
                    <option value="home">{m.contact_form_kind_home()}</option>
                    <option value="fax">{m.contact_form_kind_fax()}</option>
                    <option value="other">{m.contact_form_kind_other()}</option>
                  </select>
                  <input
                    class="input flex-1"
                    bind:value={phone.value}
                    placeholder="+1 555 0100"
                  />
                  <button
                    type="button"
                    class="text-xs text-error-500 hover:underline"
                    onclick={() => removePhone(i)}
                  >{m.contact_form_button_remove()}</button>
                </div>
              {/each}
            </div>

            <div class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium">{m.contact_form_label_impp()}</span>
                <button type="button" class="btn btn-sm preset-tonal" onclick={addImpp}>
                  {m.contact_form_button_add_impp()}
                </button>
              </div>
              {#each formImpp as im, i (i)}
                <div class="flex items-center gap-2">
                  <select class="select w-32" bind:value={im.kind}>
                    <option value="matrix">{m.contact_form_kind_matrix()}</option>
                    <option value="xmpp">{m.contact_form_kind_xmpp()}</option>
                    <option value="telegram">{m.contact_form_kind_telegram()}</option>
                    <option value="signal">{m.contact_form_kind_signal()}</option>
                    <option value="skype">{m.contact_form_kind_skype()}</option>
                    <option value="whatsapp">{m.contact_form_kind_whatsapp()}</option>
                    <option value="other">{m.contact_form_kind_other()}</option>
                  </select>
                  <input
                    class="input flex-1"
                    bind:value={im.value}
                    placeholder={m.contact_form_placeholder_impp()}
                  />
                  <button
                    type="button"
                    class="text-xs text-error-500 hover:underline"
                    onclick={() => removeImpp(i)}
                  >{m.contact_form_button_remove()}</button>
                </div>
              {/each}
            </div>
          </div>
        </details>

        <!-- ── Work ─────────────────────────────────────────── -->
        <details class="contact-section">
          <summary class="contact-section-summary">{m.contact_form_section_work()}</summary>
          <div class="contact-section-body">
            <div class="grid grid-cols-2 gap-3">
              <label class="label">
                <span>{m.contact_form_label_organization()}</span>
                <input class="input" bind:value={formOrg} placeholder="Example Corp" />
              </label>
              <label class="label">
                <span>{m.contact_form_label_job_title()}</span>
                <input class="input" bind:value={formTitle} placeholder="Product Manager" />
              </label>
            </div>
            <label class="label">
              <span>{m.contact_form_label_role()} <span class="text-surface-500">({m.contact_form_hint_role()})</span></span>
              <input class="input" bind:value={formRole} placeholder="Project Lead" />
            </label>
            <div>
              <span class="text-sm font-medium mb-1 block">{m.contact_form_label_categories()}</span>
              <div class="flex flex-wrap gap-1.5 mb-2">
                {#each formCategories as cat, i (cat)}
                  <span class="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300 text-xs">
                    {cat}
                    <button
                      type="button"
                      class="hover:text-error-500 leading-none"
                      onclick={() => removeChip(formCategories, i, (next) => (formCategories = next))}
                    >×</button>
                  </span>
                {/each}
              </div>
              <input
                class="input"
                bind:value={formCategoryDraft}
                placeholder={m.contact_form_placeholder_chip()}
                onkeydown={(e) => {
                  if (e.key === 'Enter' || e.key === ',') {
                    e.preventDefault()
                    commitChipDraft(formCategories, formCategoryDraft, (next, draft) => {
                      formCategories = next
                      formCategoryDraft = draft
                    })
                  }
                }}
              />
            </div>
          </div>
        </details>

        <!-- ── Address & Web ────────────────────────────────── -->
        <details class="contact-section">
          <summary class="contact-section-summary">{m.contact_form_section_address_web()}</summary>
          <div class="contact-section-body">
            <div class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium">{m.contact_form_label_addresses()}</span>
                <button type="button" class="btn btn-sm preset-tonal" onclick={addAddress}>
                  {m.contact_form_button_add_address()}
                </button>
              </div>
              {#each formAddresses as addr, i (i)}
                <div class="card p-3 bg-surface-50 dark:bg-surface-900/50 rounded-md space-y-2">
                  <div class="flex items-center gap-2">
                    <select class="select w-32" bind:value={addr.kind}>
                      <option value="home">{m.contact_form_kind_home()}</option>
                      <option value="work">{m.contact_form_kind_work()}</option>
                      <option value="other">{m.contact_form_kind_other()}</option>
                    </select>
                    <button
                      type="button"
                      class="ml-auto text-xs text-error-500 hover:underline"
                      onclick={() => removeAddress(i)}
                    >{m.contact_form_button_remove()}</button>
                  </div>
                  <input class="input" bind:value={addr.street} placeholder={m.contact_form_placeholder_street()} />
                  <div class="grid grid-cols-2 gap-2">
                    <input class="input" bind:value={addr.locality} placeholder={m.contact_form_placeholder_city()} />
                    <input class="input" bind:value={addr.region} placeholder={m.contact_form_placeholder_region()} />
                  </div>
                  <div class="grid grid-cols-2 gap-2">
                    <input class="input" bind:value={addr.postal_code} placeholder={m.contact_form_placeholder_postal()} />
                    <input class="input" bind:value={addr.country} placeholder={m.contact_form_placeholder_country()} />
                  </div>
                </div>
              {/each}
            </div>

            <label class="label">
              <span>{m.contact_form_label_websites()} <span class="text-surface-500">({m.contact_form_hint_websites_one_per_line()})</span></span>
              <textarea
                class="textarea"
                rows="2"
                bind:value={formUrls}
                placeholder="https://example.com"
              ></textarea>
            </label>

            <label class="label">
              <span>{m.contact_form_label_geo()} <span class="text-surface-500">({m.contact_form_hint_geo()})</span></span>
              <input
                class="input"
                bind:value={formGeo}
                placeholder="geo:48.198634,16.371648"
              />
            </label>
          </div>
        </details>

        <!-- ── Other ────────────────────────────────────────── -->
        <details class="contact-section">
          <summary class="contact-section-summary">{m.contact_form_section_other()}</summary>
          <div class="contact-section-body">
            <div>
              <span class="text-sm font-medium mb-1 block">{m.contact_form_label_languages()}</span>
              <div class="flex flex-wrap gap-1.5 mb-2">
                {#each formLanguages as lang, i (lang)}
                  <span class="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-surface-200 dark:bg-surface-700 text-xs">
                    {lang}
                    <button
                      type="button"
                      class="hover:text-error-500 leading-none"
                      onclick={() => removeChip(formLanguages, i, (next) => (formLanguages = next))}
                    >×</button>
                  </span>
                {/each}
              </div>
              <input
                class="input"
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
              />
            </div>
            <label class="label">
              <span>{m.contact_form_label_timezone()}</span>
              <input
                class="input"
                bind:value={formTimezone}
                placeholder={m.contact_form_placeholder_timezone()}
              />
            </label>
            <label class="label">
              <span>{m.contact_form_label_notes()}</span>
              <textarea
                class="textarea"
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
              <select class="select" bind:value={formAccountId} onchange={onAccountChange}>
                {#each accounts as a (a.id)}
                  <option value={a.id}>{a.display_name ?? a.username}</option>
                {/each}
              </select>
            </label>
            <label class="label">
              <span>Addressbook</span>
              <select
                class="select"
                value={formAddressbookUrl}
                onchange={onAddressbookChange}
              >
                {#each addressbooksByAccount[formAccountId] ?? [] as b (b.path)}
                  <option value={b.path}>{b.display_name ?? b.name}</option>
                {/each}
              </select>
            </label>
          </div>
        {/if}

        {#if formError}
          <p class="text-sm text-red-500">{formError}</p>
        {/if}

        <div class="flex items-center gap-2 pt-2">
          <button
            class="btn preset-filled-primary-500"
            disabled={saving}
            onclick={saveContact}
          >
            {saving ? 'Saving…' : selectedId === 'new' ? 'Create contact' : 'Save changes'}
          </button>
          <button class="btn preset-tonal" disabled={saving} onclick={cancelEdit}>
            Cancel
          </button>
          {#if selectedId !== 'new'}
            <div class="flex-1"></div>
            {#if deleteConfirm}
              <span class="text-xs text-surface-500">Really delete?</span>
              <button
                class="btn preset-filled-error-500"
                disabled={saving}
                onclick={deleteSelected}
              >
                Confirm delete
              </button>
              <button
                class="btn preset-tonal"
                disabled={saving}
                onclick={() => (deleteConfirm = false)}
              >
                Keep
              </button>
            {:else}
              <button
                class="btn preset-tonal text-red-500"
                disabled={saving}
                onclick={() => (deleteConfirm = true)}
              >
                Delete
              </button>
            {/if}
          {/if}
        </div>
      </div>
    {/if}
  </main>
</div>

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
    <div class="bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-3">New mailing list</h3>

      <label class="block text-xs text-surface-500 mb-1" for="new-ml-name">Name</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="new-ml-name"
        type="text"
        class="input w-full text-sm px-2 py-1.5 rounded-md mb-3"
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

      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={newListBusy}
          onclick={() => (newListForm = null)}
        >Cancel</button>
        <button
          class="btn preset-filled-primary-500"
          disabled={newListBusy || !newListForm.name.trim()}
          onclick={() => void commitNewMailingList()}
        >{newListBusy ? 'Creating…' : 'Create'}</button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* #143 — collapsible section in the contact form.  Default
     `<details>` chrome is browser-painted and inconsistent
     across engines; we replace it with a chevron-prefixed
     summary that flips on open + a card-styled body. */
  :global(.contact-section) {
    border: 1px solid var(--color-surface-200);
    border-radius: 0.5rem;
    background: var(--color-surface-50);
    overflow: hidden;
  }
  :global([data-mode='dark'] .contact-section) {
    border-color: var(--color-surface-700);
    background: var(--color-surface-900);
  }
  :global(.contact-section-summary) {
    cursor: pointer;
    padding: 0.625rem 0.875rem;
    font-weight: 600;
    font-size: 0.875rem;
    list-style: none;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    user-select: none;
  }
  :global(.contact-section-summary::-webkit-details-marker) {
    display: none;
  }
  :global(.contact-section-summary::before) {
    content: '▸';
    display: inline-block;
    transition: transform 120ms ease;
    color: var(--color-surface-500);
    font-size: 0.75rem;
  }
  :global(.contact-section[open] > .contact-section-summary::before) {
    transform: rotate(90deg);
  }
  :global(.contact-section-body) {
    padding: 0.75rem 0.875rem 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.875rem;
    border-top: 1px solid var(--color-surface-200);
  }
  :global([data-mode='dark'] .contact-section-body) {
    border-top-color: var(--color-surface-700);
  }
</style>
