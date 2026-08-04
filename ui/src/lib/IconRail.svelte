<script lang="ts">
  /**
   * IconRail — the always-visible left-edge shell nav.
   *
   * Two-tier layout:
   *
   * 1. Account avatars at the top (initials bubbles). Clicking one
   *    selects that account. When the user has more than one account
   *    there's also an "All" bubble that turns on unified-inbox mode
   *    — same semantics as the `__all__` sentinel the old Sidebar
   *    dropdown used, but expressed as a chip you can stab at from
   *    any view.
   *
   * 2. View nav below a divider: Mail, Contacts, Calendar, Files,
   *    Talk — and Settings pinned at the bottom via a `mt-auto`
   *    spacer. Active state highlights the current `currentView`;
   *    clicks drop back to the parent via `onselectview`.
   *
   * The rail is mounted once at the App-shell level (outside any
   *  `currentView` branch) so switching between Mail and the
   *  integration views doesn't unmount / remount the rail — avatars,
   *  Talk unread-badge polling, and the active-view ring all stay
   *  smooth across navigation.
   *
   * Talk unread polling lives here (moved from the old Sidebar)
   * because the Talk badge belongs to the rail's Talk icon now; the
   * previous home inside Sidebar was only correct when the Sidebar
   * was the single surface showing the integration nav.
   */

  import { isNextcloudSource } from './ncSources'
  import type { UnlistenFn } from '@tauri-apps/api/event'
  import * as api from './api'
  import { onDestroy } from 'svelte'
  import Icon, { type IconName } from './Icon.svelte'

  interface Account {
    id: string
    display_name: string
    email: string
    /** Optional emoji avatar (issue #115) — replaces the
     *  initials bubble when set. */
    emoji?: string | null
    /** Display order for the rail; lower values render first. */
    sort_order?: number
  }

  interface TalkRoomSummary {
    unread_messages: number
    unread_mention: boolean
  }

  /** The view-nav slots the rail surfaces. Mirrors App's `View`
   *  enum for the branches the rail can reach — `loading` / `setup`
   *  are shell-level states, not rail destinations. */
  export type RailView =
    | 'inbox'
    | 'contacts'
    | 'calendar'
    | 'files'
    | 'shares'
    | 'talk'
    | 'notes'
    | 'tasks'
    | 'settings'

  interface Props {
    accounts: Account[]
    /** The currently-active mail account's id. Drives the avatar
     *  ring unless `unified` is on. */
    accountId: string | null
    /** Unified-inbox mode. When true the "All" bubble wears the
     *  active ring instead of any individual avatar. */
    unified: boolean
    /** Current view. Used to ring the matching nav icon. */
    currentView: RailView | 'loading' | 'setup'
    /** Real account id → switch to that account; sentinel
     *  `'__all__'` → turn on unified-inbox mode. Same contract the
     *  old Sidebar dropdown used. */
    onselectaccount: (id: string) => void
    onselectview: (view: RailView) => void
    /** True while a network refresh is in flight in the active
     *  mail pane (MailList or MailView).  Drives a calm spinner
     *  ring overlay on the active account's avatar so the
     *  "Refreshing…" hint lives somewhere persistent rather
     *  than as a buggy strip inside the list (#161). */
    mailRefreshing?: boolean
    /** Aggregated Nextcloud capability snapshot (#189).  Each
     *  flag is true when at least one connected NC account
     *  exposes that feature.  `hasAny` short-circuits the
     *  whole integration block — without a Nextcloud account
     *  configured at all, none of the per-feature icons make
     *  sense.  Defaults to "everything available" so the rail
     *  doesn't accidentally hide things during the brief
     *  capability-load window. */
    ncCaps?: {
      hasAny: boolean
      contacts: boolean
      calendar: boolean
      files: boolean
      talk: boolean
      notes: boolean
      tasks: boolean
    }
  }
  let {
    accounts,
    accountId,
    unified,
    currentView,
    onselectaccount,
    onselectview,
    mailRefreshing = false,
    ncCaps = {
      hasAny: true,
      contacts: true,
      calendar: true,
      files: true,
      talk: true,
      notes: true,
      tasks: true,
    },
  }: Props = $props()

  /** First-letter / initial-pair fallback for the avatar bubble.
   *  Mirrors the pattern AddressAutocomplete uses for contact rows
   *  so single-word and multi-word display names both look right. */
  function initials(a: Account): string {
    const src = (a.display_name || a.email || '').trim()
    if (!src) return '?'
    const parts = src.split(/\s+/).filter(Boolean)
    if (parts.length === 1) return parts[0][0].toUpperCase()
    return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase()
  }

  /** Accounts sorted by `sort_order` (#115) so the rail honours
   *  the user's chosen ordering — `id` ties keep the order
   *  stable when two rows share the same sort_order. */
  const sortedAccounts = $derived(
    [...accounts].sort((a, b) => {
      const ao = a.sort_order ?? 0
      const bo = b.sort_order ?? 0
      if (ao !== bo) return ao - bo
      return a.id.localeCompare(b.id)
    }),
  )

  // ── Per-account unread badges (#115) ────────────────────────
  // The Rust side emits `unread-count-by-account-updated` after
  // every poll, carrying a HashMap<accountId, count>.  We seed
  // the state once on mount via `get_unread_counts_by_account`
  // so the badge paints immediately, then keep it live with the
  // event subscription.
  let unreadByAccount = $state<Record<string, number>>({})
  $effect(() => {
    void api.mail.getUnreadCountsByAccount()
      .then((m) => (unreadByAccount = m))
      .catch((e) => console.warn('get_unread_counts_by_account failed', e))
    let unlisten: UnlistenFn | null = null
    void api.onAppEvent(
      'unread-count-by-account-updated',
      (e) => {
        unreadByAccount = e.payload ?? {}
      },
    )
      .then((fn) => (unlisten = fn))
      .catch((e) =>
        console.warn('listen unread-count-by-account-updated failed', e),
      )
    return () => {
      if (unlisten) unlisten()
    }
  })
  function unreadFor(id: string): number {
    return unreadByAccount[id] ?? 0
  }
  /** Sum of every account's unread count — drives the "All
   *  inboxes" bubble's badge so the unified view also shows a
   *  single aggregate red dot when anything's pending. */
  const totalUnread = $derived(
    Object.values(unreadByAccount).reduce((a, b) => a + b, 0),
  )

  // ── Talk unread badge ───────────────────────────────────────
  // Polls the first configured Nextcloud account every 30s and
  // aggregates unread counts + mention state across rooms. Same
  // logic that used to live in Sidebar.svelte — follows the Talk
  // rail icon now.
  let talkUnreadTotal = $state(0)
  let talkUnreadHasMention = $state(false)
  const TALK_POLL_MS = 30_000
  let talkPollTimer: number | null = null

  async function refreshTalkBadge() {
    try {
      // Generic-DAV / local sources reuse the NC account record but
      // have no Talk endpoint (#413) — only poll real Nextclouds.
      const ncAccounts = (
        await api.nextcloud.getNextcloudAccounts()
      ).filter(isNextcloudSource)
      if (ncAccounts.length === 0) {
        talkUnreadTotal = 0
        talkUnreadHasMention = false
        return
      }
      const rooms = await api.talk.listTalkRooms({
        ncId: ncAccounts[0].id,
      })
      let total = 0
      let mention = false
      for (const r of rooms) {
        total += r.unread_messages
        if (r.unread_mention && r.unread_messages > 0) mention = true
      }
      talkUnreadTotal = total
      talkUnreadHasMention = mention
    } catch (e) {
      console.warn('Talk unread poll failed:', e)
    }
  }

  $effect(() => {
    void refreshTalkBadge()
    talkPollTimer = window.setInterval(refreshTalkBadge, TALK_POLL_MS)
    return () => {
      if (talkPollTimer !== null) window.clearInterval(talkPollTimer)
      talkPollTimer = null
    }
  })

  onDestroy(() => {
    if (talkPollTimer !== null) window.clearInterval(talkPollTimer)
  })

  /** Nav entries rendered below the divider. `match` is the
   *  `currentView` value that should light the ring. Kept as a
   *  plain array so re-ordering / renaming is a one-line edit. */
  interface NavEntry {
    match: RailView
    label: string
    icon: IconName
  }
  // No 'inbox' entry here (#161 follow-up): the account avatars
  // above the divider already navigate to mail, so a dedicated
  // mail icon would be redundant.  Use `onselectaccount` (with the
  // `'__all__'` sentinel for unified mode) to land in the inbox.
  // Source-of-truth nav list.  Visibility is gated per-entry
  // by the matching `ncCaps` flag below; the static list keeps
  // the order canonical so we don't end up with the icons
  // shuffling when capabilities flip.
  const MAIN_NAV: NavEntry[] = [
    { match: 'contacts', label: 'Contacts', icon: 'contacts' },
    { match: 'calendar', label: 'Calendar', icon: 'calendar' },
    { match: 'files', label: 'Files', icon: 'files' },
    { match: 'shares', label: 'Share links', icon: 'share-links' },
    { match: 'talk', label: 'Talk', icon: 'meetings' },
    { match: 'tasks', label: 'Tasks', icon: 'tasks' },
    { match: 'notes', label: 'Notes', icon: 'notes' },
  ]

  /** Filtered view of `MAIN_NAV` honouring the Nextcloud
   *  capability snapshot (#189).  When the user has no NC
   *  account at all (`hasAny=false`) the whole block is empty,
   *  matching the issue's "hide features that aren't available
   *  through Nextcloud" ask.  When they have an account but
   *  Talk / Notes etc. aren't installed on the server, those
   *  rows drop out individually. */
  const visibleNav = $derived(
    !ncCaps.hasAny
      ? []
      : MAIN_NAV.filter((entry) => {
          switch (entry.match) {
            case 'contacts': return ncCaps.contacts
            case 'calendar': return ncCaps.calendar
            case 'files':    return ncCaps.files
            case 'shares':   return ncCaps.files
            case 'talk':     return ncCaps.talk
            case 'notes':    return ncCaps.notes
            case 'tasks':    return ncCaps.tasks
            default:         return true
          }
        }),
  )
</script>

<!-- `overflow-y-auto` (#454): on a vertically-small window the rail
     used to clip its lower icons (and the pinned Settings button)
     with no way to reach them — the app shell hides overflow.
     Scrolling keeps every destination reachable; `shrink-0` on the
     items stops the flex column from squashing the 36px bubbles
     before the scrollbar kicks in. -->
<aside
  class="w-14 shrink-0 border-r glass-panel flex flex-col items-center py-2 gap-3 overflow-y-auto"
>
  <!-- Account avatars. The "All" bubble only appears when the user
       has more than one account — for a single-account setup it's
       chrome with no distinct behaviour. -->
  {#if accounts.length > 1}
    {@const allActive = unified && currentView === 'inbox'}
    <button
      class="relative w-9 h-9 shrink-0 rounded-full flex items-center justify-center text-xs font-semibold
             transition-colors
             {allActive
               ? 'bg-primary-500 text-white ring-2 ring-offset-2 ring-offset-surface-100 dark:ring-offset-surface-800 ring-primary-500'
               : 'bg-surface-300 dark:bg-surface-700 text-surface-700 dark:text-surface-300 hover:bg-surface-400 dark:hover:bg-surface-600'}"
      title="All inboxes (unified)"
      aria-label="All inboxes"
      onclick={() => onselectaccount('__all__')}
    >
      <!-- Inbox glyph matches the Mail rail icon + the "All
           Inboxes" folder entry, so the "this is the aggregate"
           meaning carries across every surface the user sees. The
           wrapper has a fixed 28×28 box so the composite (inbox +
           🌐 corner badge) sits as one centered unit inside the
           36×36 bubble — without the explicit size, the badge
           protrudes outside the wrapper and pulls the visual
           weight to the bottom-right. -->
      <span class="relative block w-7 h-7">
        <!-- The emoji's intrinsic font metrics put extra ascent above
             the visible glyph, so a flex-centered emoji renders low.
             A 2px upward translate lands the visible centre on the
             box's geometric centre across Linux/macOS/Windows emoji
             fonts. -->
        <span class="absolute inset-0 flex items-center justify-center"><Icon name="global-inbox" size={20} /></span>
      </span>
      {#if unified && mailRefreshing}
        <!-- Spinner first so the unread badge below paints on
             top of it — the badge is the higher-priority signal
             and the ring shouldn't ever obscure the count. -->
        <span
          class="pointer-events-none absolute inset-0 rounded-full border-2 border-transparent border-t-white/80 animate-spin"
          aria-hidden="true"
          title="Refreshing"
        ></span>
      {/if}
      {#if totalUnread > 0}
        <span
          class="absolute -top-0.5 -right-0.5 min-w-4 h-4 px-1 text-[10px] rounded-full bg-red-500 text-white flex items-center justify-center font-semibold ring-2 ring-surface-100 dark:ring-surface-800"
          title={`${totalUnread} unread across all inboxes`}
        >{totalUnread > 99 ? '99+' : totalUnread}</span>
      {/if}
    </button>
    <!-- Sub-divider between the All-inboxes bubble and the
         individual account avatars.  Visually groups "aggregate"
         vs. "single account" without making the rail noisier
         than the main divider below the avatar stack. -->
    <div class="w-6 h-px my-1 shrink-0 bg-surface-300 dark:bg-surface-600" aria-hidden="true"></div>
  {/if}
  {#each sortedAccounts as a (a.id)}
    <!-- The active ring only paints while we're actually on the
         mail view (#161 follow-up).  When the user is in
         calendar / contacts / settings, the avatars stay quiet
         so the rail clearly says "click an account to see its
         mail" rather than "this account is selected." -->
    {@const active = !unified && accountId === a.id && currentView === 'inbox'}
    {@const unread = unreadFor(a.id)}
    <button
      class="relative w-9 h-9 shrink-0 rounded-full flex items-center justify-center text-xs font-semibold
             transition-colors
             {active
               ? 'bg-primary-500 text-white ring-2 ring-offset-2 ring-offset-surface-100 dark:ring-offset-surface-800 ring-primary-500'
               : 'bg-surface-300 dark:bg-surface-700 text-surface-700 dark:text-surface-300 hover:bg-surface-400 dark:hover:bg-surface-600'}"
      title={`${a.display_name || a.email}${unread > 0 ? ` — ${unread} unread` : ''}`}
      aria-label="Switch to {a.display_name || a.email}"
      onclick={() => onselectaccount(a.id)}
    >
      {#if a.emoji && a.emoji.trim()}
        <span class="text-lg leading-none">{a.emoji}</span>
      {:else}
        {initials(a)}
      {/if}
      {#if active && mailRefreshing}
        <!-- Calm refresh hint (#161): a thin spinner ring
             overlaid on the active avatar replaces the inline
             "Refreshing…" strip that used to live in MailList /
             MailView.  Only renders for the active account so
             it doesn't compete with the avatar's content.
             Painted *before* the unread badge so the badge sits
             on top — the count is the higher-priority signal. -->
        <span
          class="pointer-events-none absolute inset-0 rounded-full border-2 border-transparent border-t-white/80 animate-spin"
          aria-hidden="true"
          title="Refreshing"
        ></span>
      {/if}
      {#if unread > 0}
        <!-- Red unread badge (#115).  Pinned top-right and
             ringed in the rail's surface colour so the badge
             reads cleanly over both light and dark themes
             without doubling as part of the avatar bubble's
             outline. -->
        <span
          class="absolute -top-0.5 -right-0.5 min-w-4 h-4 px-1 text-[10px] rounded-full bg-red-500 text-white flex items-center justify-center font-semibold ring-2 ring-surface-100 dark:ring-surface-800"
        >{unread > 99 ? '99+' : unread}</span>
      {/if}
    </button>
  {/each}

  <!-- Divider between account bubbles and the view nav.  Only
       renders when there's at least one account AND at least
       one integration icon will follow — when no Nextcloud is
       connected (#189), the integration block is empty and the
       divider has nothing to separate. -->
  {#if accounts.length > 0 && visibleNav.length > 0}
    <div class="w-6 h-px my-1 shrink-0 bg-surface-300 dark:bg-surface-600" aria-hidden="true"></div>
  {/if}

  {#each visibleNav as entry (entry.match)}
    {@const active = currentView === entry.match}
    <button
      class="w-9 h-9 shrink-0 rounded-lg flex items-center justify-center text-lg transition-colors duration-150 ease-out relative
             {active
               ? 'bg-primary-500/15 text-primary-500'
               : 'text-surface-600 dark:text-surface-300 hover:bg-primary-500/10'}"
      title={entry.label}
      aria-label={entry.label}
      data-tour="rail-nav"
      onclick={() => onselectview(entry.match)}
    >
      <Icon name={entry.icon} size={20} />
      <!-- Talk-specific unread badge, pinned to the top-right
           corner of the icon. Red tint when there's a mention,
           primary otherwise. -->
      {#if entry.match === 'talk' && talkUnreadTotal > 0}
        <span
          class="absolute -top-0.5 -right-0.5 min-w-4 h-4 px-1 text-[10px] rounded-full
                 flex items-center justify-center font-semibold
                 {talkUnreadHasMention
                   ? 'bg-red-500 text-white'
                   : 'bg-primary-500 text-white'}"
          title={talkUnreadHasMention ? 'You were mentioned' : 'Unread Talk messages'}
        >{talkUnreadTotal}</span>
      {/if}
    </button>
  {/each}

  <!-- Settings pinned to the bottom. `mt-auto` on the wrapper
       pushes the remaining flex space between the main nav and
       this slot. -->
  <div class="mt-auto shrink-0">
    <button
      class="w-9 h-9 rounded-lg flex items-center justify-center text-lg transition-colors duration-150 ease-out
             {currentView === 'settings'
               ? 'bg-primary-500/15 text-primary-500'
               : 'text-surface-600 dark:text-surface-300 hover:bg-primary-500/10'}"
      title="Settings"
      aria-label="Settings"
      data-tour="rail-settings"
      onclick={() => onselectview('settings')}
    >
      <Icon name="settings" size={20} />
    </button>
  </div>
</aside>
