<script lang="ts">
  /**
   * ProfilesSettings — manage browser-style profiles (#534).
   *
   * A profile is a fully separate storage universe (own accounts,
   * own encrypted cache, own settings) inside one install.  This
   * panel is the management surface: create, rename, re-icon,
   * delete, and pick the startup behaviour.  Switching a window's
   * profile and opening one in its own window live in the rail's
   * profile bubble (IconRail's switcher popover, #535).
   *
   * All data flows through `profileStore`; every mutation makes the
   * backend broadcast `profiles-changed`, which reloads the store —
   * so this component never patches the list locally.
   */

  import * as api from './api'
  import type { Profile, ProfileIcon, StartupMode } from './api'
  import { profileStore } from './profileStore.svelte'
  import EmojiPicker from './EmojiPicker.svelte'
  import Icon, { type IconName } from './Icon.svelte'
  import { anchorRect, clampToViewport, cursorAnchor } from './coords'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'

  const profiles = $derived(profileStore.profiles)
  const currentId = $derived(profileStore.currentId)
  const startupMode = $derived(profileStore.startupMode)

  // Fresh snapshot on panel mount — the store is already live via
  // App's init, but the user may have had the app open a while.
  $effect(() => {
    void profileStore.load()
  })

  /** One error slot for the panel; every failed mutation lands
   *  here and the next attempt clears it. */
  let opError = $state('')
  let busy = $state(false)

  // ── Row menu — three-dot and right-click share this state ────
  let contextMenu = $state<{ profile: Profile; x: number; y: number } | null>(null)

  $effect(() => {
    if (!contextMenu) return
    const onDocMouseDown = () => (contextMenu = null)
    const onDocKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') contextMenu = null
    }
    document.addEventListener('mousedown', onDocMouseDown)
    document.addEventListener('keydown', onDocKey)
    return () => {
      document.removeEventListener('mousedown', onDocMouseDown)
      document.removeEventListener('keydown', onDocKey)
    }
  })

  function openContextMenu(e: MouseEvent, profile: Profile) {
    e.preventDefault()
    renamingId = null
    contextMenu = { profile, ...clampToViewport(cursorAnchor(e), 200, 140) }
  }

  // Escape closes whichever modal is open — document-level because
  // the icon-change modal has no focused element to receive the key.
  $effect(() => {
    if (!createOpen && !iconPickerFor && !deleteConfirm) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || deleteBusy) return
      createOpen = false
      iconPickerFor = null
      deleteConfirm = null
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  })

  // ── Inline rename (Enter commits, Escape cancels, blur commits) ──
  let renamingId = $state<string | null>(null)
  let renameValue = $state('')

  function startRename(profile: Profile) {
    renamingId = profile.id
    renameValue = profile.name
  }

  async function commitRename() {
    // Re-entrancy guard: Enter→commit unmounts the input, whose
    // blur handler would otherwise commit a second time.
    if (!renamingId || busy) return
    const id = renamingId
    const name = renameValue.trim()
    const existing = profiles.find((p) => p.id === id)
    if (!existing || !name || name === existing.name) {
      renamingId = null
      return
    }
    busy = true
    opError = ''
    try {
      await api.profiles.updateProfile({ id, name })
    } catch (e) {
      opError = formatError(e)
    } finally {
      busy = false
      renamingId = null
    }
  }

  function cancelRename() {
    renamingId = null
  }

  // ── Icon choice ──────────────────────────────────────────────
  // Both surfaces (create modal + "Change icon" modal) offer the
  // same two sources: a grid of predefined icons rendered in the
  // style of the existing navigation icons, and the shared
  // EmojiPicker.  All names below are registered `IconName`s — no
  // new SVGs (see the Icon registry rule in CLAUDE.md).
  const PRESET_ICONS: IconName[] = [
    'contacts', 'group', 'team', 'address-book',
    'email-envelope', 'global-inbox', 'star', 'sun',
    'cloud', 'lock', 'notes', 'ai',
    // #552 — everyday-life profile identities
    'briefcase', 'suitcase', 'plane', 'home',
    'heart', 'graduation-cap', 'gamepad', 'music-note',
  ]

  /** Profile whose icon is being changed; null = modal closed. */
  let iconPickerFor = $state<Profile | null>(null)

  async function commitIcon(profile: Profile, icon: ProfileIcon) {
    iconPickerFor = null
    busy = true
    opError = ''
    try {
      await api.profiles.updateProfile({ id: profile.id, icon })
    } catch (e) {
      opError = formatError(e)
    } finally {
      busy = false
    }
  }

  // ── Create flow ──────────────────────────────────────────────
  let createOpen = $state(false)
  let createName = $state('')
  let createIcon = $state<ProfileIcon>({ kind: 'named', value: 'contacts' })

  function openCreate() {
    createName = ''
    createIcon = { kind: 'named', value: 'contacts' }
    createOpen = true
  }

  async function confirmCreate() {
    const name = createName.trim()
    if (!name || busy) return
    busy = true
    opError = ''
    try {
      await api.profiles.createProfile({ name, icon: $state.snapshot(createIcon) })
      createOpen = false
    } catch (e) {
      opError = formatError(e)
    } finally {
      busy = false
    }
  }

  // ── Delete flow ──────────────────────────────────────────────
  /** The backend refuses these too — mirroring the policy here
   *  lets the menu disable the item with a tooltip instead of
   *  surfacing a rejection after the click. */
  function deleteBlockedReason(profile: Profile): string {
    if (profiles.length <= 1) return m.profiles_delete_last_hint()
    if (profile.id === currentId) return m.profiles_delete_current_hint()
    return ''
  }

  let deleteConfirm = $state<Profile | null>(null)
  let deleteBusy = $state(false)
  let deleteError = $state('')

  async function confirmDelete() {
    if (!deleteConfirm || deleteBusy) return
    deleteBusy = true
    deleteError = ''
    try {
      await api.profiles.deleteProfile({ id: deleteConfirm.id })
      deleteConfirm = null
    } catch (e) {
      deleteError = formatError(e)
    } finally {
      deleteBusy = false
    }
  }

  // ── Startup mode ─────────────────────────────────────────────
  const startupKind = $derived(startupMode.mode)
  /** Which profile the "fixed" secondary select shows: the pinned
   *  one, else a sensible default for the moment the user switches
   *  the primary select over to "fixed". */
  const fixedProfileId = $derived(
    startupMode.mode === 'fixed'
      ? startupMode.id
      : (currentId ?? profiles[0]?.id ?? ''),
  )

  async function applyStartupMode(mode: StartupMode) {
    opError = ''
    try {
      await api.profiles.setStartupMode({ mode })
    } catch (e) {
      opError = formatError(e)
    }
  }

  function onStartupKindChange(kind: string) {
    if (kind === 'fixed') {
      if (fixedProfileId) void applyStartupMode({ mode: 'fixed', id: fixedProfileId })
    } else if (kind === 'all') {
      void applyStartupMode({ mode: 'all' })
    } else {
      void applyStartupMode({ mode: 'last_used' })
    }
  }
</script>

<section class="space-y-6">
  <header>
    <h2 class="text-xl font-semibold">{m.profiles_title()}</h2>
    <p class="text-sm text-surface-500 mt-1 max-w-xl">
      {m.profiles_intro()}
    </p>
  </header>

  {#if opError}
    <div class="text-sm text-red-500 p-4 bg-red-500/10 rounded-lg">
      {opError}
    </div>
  {/if}

  <!-- Profile list -->
  <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-2xl">
    <div class="space-y-0.5" role="list">
      {#each profiles as profile (profile.id)}
        {#if renamingId === profile.id}
          <div class="flex items-center gap-3 px-3 py-2">
            {@render iconBubble(profile.icon)}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              type="text"
              class="input flex-1 text-sm px-2 py-1 rounded-lg"
              bind:value={renameValue}
              disabled={busy}
              autofocus
              onkeydown={(e) => {
                if (e.key === 'Enter') { e.preventDefault(); void commitRename() }
                else if (e.key === 'Escape') { e.preventDefault(); cancelRename() }
              }}
              onblur={() => { if (renamingId) void commitRename() }}
            />
          </div>
        {:else}
          <div
            class="group flex items-center gap-3 px-3 py-2 rounded-lg transition-colors duration-150 ease-out hover:bg-primary-500/10"
            oncontextmenu={(e) => openContextMenu(e, profile)}
            role="listitem"
          >
            {@render iconBubble(profile.icon)}
            <span class="flex-1 min-w-0 truncate text-sm">{profile.name}</span>
            {#if profile.id === currentId}
              <span class="badge preset-tonal-primary text-xs shrink-0">{m.profiles_current_badge()}</span>
            {/if}
            <!-- Three-dot trigger — mirrors the right-click menu so
                 trackpad / touchscreen users get the same actions. -->
            <button
              class="w-5 h-5 rounded-lg text-surface-500 hover:bg-primary-500/10 transition-colors duration-150 ease-out leading-none shrink-0
                     {contextMenu?.profile.id === profile.id
                       ? 'opacity-100'
                       : 'opacity-0 group-hover:opacity-100 focus:opacity-100'}"
              title={m.profiles_row_actions()}
              aria-label={m.profiles_row_actions()}
              onclick={(e) => {
                e.stopPropagation()
                const r = anchorRect(e.currentTarget as HTMLElement)
                contextMenu = { profile, ...clampToViewport({ x: r.right + 4, y: r.top }, 200, 140) }
              }}
            >⋯</button>
          </div>
        {/if}
      {/each}
    </div>

    <div class="mt-4">
      <button
        class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center gap-1.5"
        disabled={busy}
        onclick={openCreate}
      >
        <Icon name="plus" size={14} />
        <span>{m.profiles_new_button()}</span>
      </button>
    </div>
  </div>

  <!-- Startup behaviour -->
  <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-2xl">
    <h3 class="text-base font-semibold mb-1">{m.profiles_startup_title()}</h3>
    <p class="text-xs text-surface-400 mb-3 max-w-xl">
      {m.profiles_startup_hint()}
    </p>
    <div class="space-y-2 text-sm">
      <select
        class="select px-2 py-1 text-sm rounded-lg max-w-80"
        value={startupKind}
        onchange={(e) => onStartupKindChange((e.currentTarget as HTMLSelectElement).value)}
      >
        <option value="last_used">{m.profiles_startup_last_used()}</option>
        <option value="fixed">{m.profiles_startup_fixed()}</option>
        <option value="all">{m.profiles_startup_all()}</option>
      </select>
      {#if startupKind === 'fixed'}
        <label class="flex items-center gap-2">
          <span class="text-xs text-surface-500 shrink-0">{m.profiles_startup_fixed_label()}</span>
          <select
            class="select px-2 py-1 text-sm rounded-lg max-w-65"
            value={fixedProfileId}
            onchange={(e) =>
              void applyStartupMode({
                mode: 'fixed',
                id: (e.currentTarget as HTMLSelectElement).value,
              })}
          >
            {#each profiles as p (p.id)}
              <option value={p.id}>{p.name}</option>
            {/each}
          </select>
        </label>
      {/if}
    </div>
  </div>

  <!-- Chunk-4 forward pointer: windows/switching aren't here yet. -->
  <p class="text-xs text-surface-400 max-w-xl">
    {m.profiles_note_switching()}
  </p>
</section>

{#snippet iconBubble(icon: ProfileIcon)}
  <span class="w-9 h-9 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300 flex items-center justify-center shrink-0">
    {#if icon.kind === 'emoji'}
      <span class="text-lg leading-none">{icon.value}</span>
    {:else}
      <Icon name={icon.value as IconName} size={18} />
    {/if}
  </span>
{/snippet}

<!-- Shared row menu — one component behind both the ⋯ button and
     right-click, per the house menu contract. -->
{#if contextMenu}
  {@const blocked = deleteBlockedReason(contextMenu.profile)}
  <div
    class="fixed z-60 min-w-44 rounded-xl glass-float py-1 text-sm"
    style="left: {contextMenu.x}px; top: {contextMenu.y}px;"
    role="menu"
    tabindex="-1"
    onmousedown={(e) => e.stopPropagation()}
  >
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={busy}
      onclick={() => {
        const p = contextMenu!.profile
        contextMenu = null
        startRename(p)
      }}
    >
      <Icon name="compose" size={16} />
      <span>{m.profiles_menu_rename()}</span>
    </button>
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={busy}
      onclick={() => {
        const p = contextMenu!.profile
        contextMenu = null
        iconPickerFor = p
      }}
    >
      <Icon name="emoji" size={16} />
      <span>{m.profiles_menu_change_icon()}</span>
    </button>
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-red-500/10 transition-colors duration-150 ease-out text-red-600 dark:text-red-400 disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={busy || blocked !== ''}
      title={blocked}
      onclick={() => {
        const p = contextMenu!.profile
        contextMenu = null
        deleteError = ''
        deleteConfirm = p
      }}
    >
      <Icon name="trash" size={16} />
      <span>{m.profiles_menu_delete()}</span>
    </button>
  </div>
{/if}

<!-- Create modal — modals are reserved for create flows and
     destructive confirms; rename stays inline. -->
{#if createOpen}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) createOpen = false }}
  >
    <div class="glass-float rounded-2xl w-[28rem] max-w-full p-5 max-h-[85vh] overflow-y-auto">
      <h3 class="text-base font-semibold mb-3 text-on-glass">{m.profiles_create_title()}</h3>

      <label class="block mb-4">
        <span class="text-xs text-on-glass-muted block mb-1">{m.profiles_name_label()}</span>
        <!-- svelte-ignore a11y_autofocus -->
        <input
          type="text"
          class="input w-full text-sm px-2 py-1 rounded-lg"
          placeholder={m.profiles_name_placeholder()}
          bind:value={createName}
          autofocus
          onkeydown={(e) => { if (e.key === 'Enter') { e.preventDefault(); void confirmCreate() } }}
        />
      </label>

      <span class="text-xs text-on-glass-muted block mb-1">{m.profiles_icon_label()}</span>
      {@render iconChooser(
        createIcon,
        (icon) => (createIcon = icon),
      )}

      <!-- Icon-only confirm / cancel pair, per the house form
           vocabulary: `save-draft` (→ `loading` in flight) confirms,
           `close` cancels.  Same shape as the create-folder row in
           NextcloudFileBrowser. -->
      <div class="flex justify-end gap-2 mt-4">
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
          disabled={busy}
          onclick={() => (createOpen = false)}
          title={m.profiles_create_cancel()}
          aria-label={m.profiles_create_cancel()}
        ><Icon name="close" size={14} /></button>
        <button
          class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
          disabled={busy || createName.trim() === ''}
          onclick={() => void confirmCreate()}
          title={m.profiles_create_confirm()}
          aria-label={m.profiles_create_confirm()}
        ><Icon name={busy ? 'loading' : 'save-draft'} size={14} /></button>
      </div>
    </div>
  </div>
{/if}

<!-- Change-icon modal — same chooser as the create flow. -->
{#if iconPickerFor}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) iconPickerFor = null }}
  >
    <div class="glass-float rounded-2xl w-[28rem] max-w-full p-5 max-h-[85vh] overflow-y-auto">
      <h3 class="text-base font-semibold mb-1 text-on-glass">{m.profiles_change_icon_title()}</h3>
      <p class="text-xs text-on-glass-muted mb-3">
        {iconPickerFor.name}
      </p>
      {@render iconChooser(
        iconPickerFor.icon,
        (icon) => void commitIcon(iconPickerFor!, icon),
      )}
    </div>
  </div>
{/if}

{#snippet iconChooser(selected: ProfileIcon, onpick: (icon: ProfileIcon) => void)}
  <!-- Predefined icons, rendered in the style of the existing
       navigation icons … -->
  <div class="flex flex-wrap gap-1 mb-3">
    {#each PRESET_ICONS as name (name)}
      {@const active = selected.kind === 'named' && selected.value === name}
      <button
        type="button"
        class="w-9 h-9 rounded-lg flex items-center justify-center transition-colors duration-150 ease-out
               {active
                 ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 ring-1 ring-inset ring-primary-500/30'
                 : 'hover:bg-primary-500/10'}"
        title={name}
        aria-label={name}
        aria-pressed={active}
        onclick={() => onpick({ kind: 'named', value: name })}
      >
        <Icon {name} size={18} />
      </button>
    {/each}
  </div>
  <!-- … or any emoji via the shared picker.  Stays opaque inside
       the glass modal (no stacked backdrop-filter). -->
  <span class="text-xs text-on-glass-muted block mb-1">{m.profiles_icon_emoji_label()}</span>
  <EmojiPicker
    value={selected.kind === 'emoji' ? selected.value : null}
    widthClass="w-full"
    allowClear={false}
    onpick={(emoji) => { if (emoji) onpick({ kind: 'emoji', value: emoji }) }}
  />
{/snippet}

<!-- Delete confirm — destructive ops always pass through an
     explicit confirm that spells out what is lost. -->
{#if deleteConfirm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget && !deleteBusy) deleteConfirm = null }}
  >
    <div class="glass-float rounded-2xl w-[28rem] max-w-full p-5">
      <h3 class="text-base font-semibold mb-2 text-on-glass">{m.profiles_delete_title()}</h3>
      <p class="text-sm text-on-glass mb-4">
        {m.profiles_delete_body({ name: deleteConfirm.name })}
      </p>
      {#if deleteError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{deleteError}</p>
      {/if}
      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={deleteBusy}
          onclick={() => (deleteConfirm = null)}
        >{m.profiles_delete_cancel()}</button>
        <button
          class="btn preset-filled-error-500"
          disabled={deleteBusy}
          onclick={() => void confirmDelete()}
        >{deleteBusy ? m.profiles_delete_busy() : m.profiles_delete_confirm()}</button>
      </div>
    </div>
  </div>
{/if}
