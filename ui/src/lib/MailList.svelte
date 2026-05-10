<script lang="ts">
  /**
   * MailList — the middle panel listing message envelopes for a folder.
   *
   * On mount (and whenever the account/folder changes) it calls the
   * `fetch_envelopes` Tauri command, which opens an IMAP connection,
   * selects the folder, and fetches the newest N envelopes.
   *
   * Envelopes are lightweight — just sender, subject, date, flags —
   * which is why they're fast enough to list. Clicking a row fires
   * `onselect(uid)` so the parent can swap MailView to that message.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import { openMailInStandaloneWindow } from './standaloneMailWindow'
  import MoveFolderPicker from './MoveFolderPicker.svelte'
  import Icon from './Icon.svelte'

  // ── Props ───────────────────────────────────────────────────
  interface EmailEnvelope {
    uid: number
    folder: string
    from: string
    subject: string
    date: string      // RFC 3339 string (serde serialises DateTime<Utc> this way)
    is_read: boolean
    is_starred: boolean
    /** IMAP `\Answered` flag (#255).  Drives the generic-reply
     *  fallback icon — true when *anyone* (Nimbus, another
     *  client, the user's phone) has answered the message. */
    is_answered?: boolean
    /** Nimbus-only reply kind (#255): `'reply'`, `'reply-all'`,
     *  `'meeting'`.  Stamped by the send path; takes precedence
     *  over `is_answered` for the icon decision. */
    replied_kind?: string | null
    /** Owning account id. Always populated for envelopes read out of
        the cache; left empty for envelopes coming straight from the
        IMAP/JMAP clients (those paths don't surface to the UI). */
    account_id: string
    /** RFC 5322 threading anchors (#277).  Optional — older
     *  cached envelopes predate the parser. */
    message_id?: string | null
    in_reply_to?: string | null
    references_ids?: string[]
  }

  /** Slim account row used to render the account label on each row in
      unified mode. We only need the id + display info. */
  interface Account {
    id: string
    display_name: string
    email: string
  }

  interface Props {
    /** Required when `unified` is true; otherwise unused. The list
        looks up each row's `account_id` here to render a short label. */
    accounts?: Account[]
    accountId: string
    folder?: string
    /** Aggregate INBOX across every account instead of fetching for a
        single account. The list shows an extra account label per row
        and reports the row's `account_id` back through `onselect`. */
    unified?: boolean
    selectedUid: number | null
    /** Bumped by the parent to force a network re-fetch (manual refresh). */
    refreshToken?: number
    /** `accountId` is passed back when in unified mode so the parent
        can route the open-message action to the right account. In
        single-account mode it's omitted (the active account is implicit). */
    onselect: (uid: number, accountId?: string) => void
    /** Bindable mirror of the rendered envelope list.  Lets the
        parent peek at "what's currently shown" without re-fetching —
        used by the auto-advance-after-delete logic to pick the next
        UID below the removed row. */
    envelopes?: EmailEnvelope[]
    /** Fires after the right-click "Move to folder…" picker (#89)
        successfully moves a message.  Same shape as `onmessagemoved`
        in `Sidebar`: parent uses it to drop the source-folder
        envelope and run the auto-advance flow when the moved row
        was the currently-open one. */
    onmessagemoved?: (removedUid: number) => void
    /** Bindable hint to the parent that the network refresh after
     *  the cache-paint is still in flight.  `App.svelte` ORs this
     *  with MailView's flag and surfaces it as a calm spinner on
     *  the active-account avatar in the IconRail (#161) — the
     *  inline "Refreshing…" strip used to live here. */
    refreshing?: boolean
  }
  let {
    accounts = [],
    accountId,
    folder = 'INBOX',
    unified = false,
    selectedUid,
    refreshToken = 0,
    onselect,
    envelopes = $bindable([]),
    onmessagemoved,
    refreshing = $bindable(false),
  }: Props = $props()

  // ── Conversation-view grouping (#277) ───────────────────────
  // Bundles every envelope that shares an RFC 5322 thread root
  // into a single inbox row, the way iPhone Mail / Thunderbird
  // do.  The thread head is the *newest* message; siblings are
  // hidden until the user clicks the count chevron.
  //
  // `threadKeyOf` picks the most-stable identifier we have:
  //
  //   1. `references_ids[0]` — the chain's root, when this is a
  //      reply.  Two messages whose `References:` headers both
  //      start with `<root>` belong to the same thread, full stop.
  //   2. `message_id` — for top-of-thread originals.  Future
  //      replies to this mail will carry it as their first
  //      `References:` entry, so siblings still resolve correctly.
  //   3. `__solo:{account}:{uid}` — fallback for envelopes that
  //      pre-date the v31 schema migration (no parsed headers
  //      yet) or for one-off mails the server didn't tag.  Each
  //      gets its own bucket → behaves like the old flat list.
  let expandedThreads = $state<Set<string>>(new Set())

  function threadKeyOf(env: EmailEnvelope): string {
    if (env.references_ids && env.references_ids.length > 0) {
      return env.references_ids[0]
    }
    if (env.message_id) {
      return env.message_id
    }
    return `__solo:${env.account_id}:${env.uid}`
  }

  /** JWZ-style canonical subject: strip a leading reply / forward
   *  prefix and collapse whitespace so `"Re: Re: Test 3"` and
   *  `"Test 3 "` produce the same key (#277).
   *
   *  We strip iteratively (`Re: Re: …` happens) and match the
   *  most common prefixes across locales — `Re:`, `Fwd:`, `Fw:`,
   *  `AW:` (German), `WG:` (German forward), `SV:` (Swedish). */
  function canonicalSubject(s: string): string {
    let out = (s || '').trim()
    const prefixRe = /^(re|fwd?|aw|wg|sv)\s*(\[\d+\])?\s*:\s*/i
    while (prefixRe.test(out)) {
      out = out.replace(prefixRe, '').trim()
    }
    // Collapse interior whitespace runs to a single space so
    // double-spaces from copy-paste don't break matching.
    return out.replace(/\s+/g, ' ').toLowerCase()
  }

  /** Subject-based merge of buckets whose explicit-anchor chains
   *  are broken (#277).  After the first reference-keyed bucketing
   *  pass, walk the heads of every bucket: if a bucket's head is
   *  a reply (`subject.startsWith("Re:") || …`) AND its canonical
   *  subject equals another bucket's head canonical subject, the
   *  two are very likely the same thread that just lost its
   *  Message-ID anchor across the wire.  Merge them.
   *
   *  Floor of 4 chars on the canonical subject so trivial subjects
   *  (`"hi"`, `"?"`) don't cause a merge cascade.  Everything else
   *  Apple Mail / Thunderbird / Outlook also fall back to this
   *  rule — see [JWZ threading
   *  §5.B.iii](https://www.jwz.org/doc/threading.html). */
  function isReplyOrForward(subject: string): boolean {
    return /^(re|fwd?|aw|wg|sv)\s*(\[\d+\])?\s*:/i.test(subject || '')
  }

  function toggleThread(key: string) {
    // Re-assign so Svelte 5 picks up the mutation — Set
    // mutations alone don't trigger reactivity.
    const next = new Set(expandedThreads)
    if (next.has(key)) next.delete(key)
    else next.add(key)
    expandedThreads = next
  }

  /** One row to actually paint.  Heads carry `siblingCount`
   *  and a fresh `threadKey`; siblings carry `siblingCount=0`
   *  and the same key as their head so the visual indent +
   *  toggle button can find each other.  `isLastSibling`
   *  marks the bottom-most child of an expanded thread —
   *  used to clip the dotted-line connector at the dot
   *  instead of letting it run to the row's bottom edge. */
  type RenderRow = {
    env: EmailEnvelope
    siblingCount: number
    isSibling: boolean
    isLastSibling: boolean
    threadKey: string
  }

  let renderRows = $derived.by((): RenderRow[] => {
    // Bucket envelopes by thread key, preserving the bucket
    // order in which the *first* member appears (envelopes are
    // already date-sorted newest-first).
    const groups = new Map<string, EmailEnvelope[]>()
    for (const env of envelopes) {
      const key = threadKeyOf(env)
      const arr = groups.get(key)
      if (arr) arr.push(env)
      else groups.set(key, [env])
    }
    // Each bucket newest-first too — usually a no-op because
    // envelopes are already in that order, but explicit is
    // safer when an out-of-order arrival lands later.
    for (const arr of groups.values()) {
      arr.sort((a, b) => new Date(b.date).getTime() - new Date(a.date).getTime())
    }

    // JWZ-style subject-based merge pass (#277).  Some servers
    // rewrite Message-IDs on delivery (local-SMTP test rigs and
    // a few Exchange configs we've seen) so the inbox copy
    // anchors a different ID than the reply's `In-Reply-To`
    // points at.  This is exactly the case where Thunderbird /
    // Apple Mail still thread correctly — they fall back to
    // subject matching.  Match Nimbus to that behaviour.
    //
    // For every reply-shaped bucket head (`Re:` / `Fwd:` / …),
    // look for another bucket whose head has the same canonical
    // subject; merge the reply bucket into the older bucket.
    //
    // `byCanonicalRoot` indexes the *non-reply* heads
    // (potential thread roots) so the lookup is O(1) per
    // bucket.  The 4-char floor avoids cascading merges on
    // trivial subjects like `"hi"`.
    const byCanonicalRoot = new Map<string, string>() // canon → group key
    for (const [key, arr] of groups) {
      const head = arr[arr.length - 1] // oldest in bucket = candidate root
      if (!head || isReplyOrForward(head.subject)) continue
      const canon = canonicalSubject(head.subject)
      if (canon.length < 4) continue
      // Ties: first non-reply bucket wins; later collisions
      // stay separate to avoid mass merges on common subjects.
      if (!byCanonicalRoot.has(canon)) byCanonicalRoot.set(canon, key)
    }
    // `keyRedirect` maps a now-merged-away key to its merge
    // target so the seen-key bookkeeping below still resolves
    // to a group when an envelope's own `threadKey` was the
    // merged-away one.
    const keyRedirect = new Map<string, string>()
    const mergedAway = new Set<string>()
    for (const [key, arr] of groups) {
      if (mergedAway.has(key)) continue
      const head = arr[arr.length - 1]
      if (!head || !isReplyOrForward(head.subject)) continue
      const canon = canonicalSubject(head.subject)
      if (canon.length < 4) continue
      const targetKey = byCanonicalRoot.get(canon)
      if (!targetKey || targetKey === key) continue
      // Move every envelope into the target bucket.
      const target = groups.get(targetKey)
      if (!target) continue
      target.push(...arr)
      target.sort(
        (a, b) => new Date(b.date).getTime() - new Date(a.date).getTime(),
      )
      mergedAway.add(key)
      keyRedirect.set(key, targetKey)
    }
    for (const key of mergedAway) groups.delete(key)

    /** Resolve an envelope's natural threadKey through the
     *  redirect chain to the post-merge canonical key. */
    function effectiveKey(env: EmailEnvelope): string {
      let key = threadKeyOf(env)
      // Defensive while-loop in case a redirect chain went two
      // hops (shouldn't happen with the single-pass merge, but
      // cheap insurance).
      let hops = 0
      while (keyRedirect.has(key) && hops < 8) {
        key = keyRedirect.get(key)!
        hops++
      }
      return key
    }
    const out: RenderRow[] = []
    const seen = new Set<string>()
    for (const env of envelopes) {
      const key = effectiveKey(env)
      if (seen.has(key)) continue
      seen.add(key)
      const group = groups.get(key)
      if (!group) continue
      const head = group[0]
      const siblings = group.slice(1)
      out.push({
        env: head,
        siblingCount: siblings.length,
        isSibling: false,
        isLastSibling: false,
        threadKey: key,
      })
      if (expandedThreads.has(key)) {
        for (let i = 0; i < siblings.length; i++) {
          out.push({
            env: siblings[i],
            siblingCount: 0,
            isSibling: true,
            isLastSibling: i === siblings.length - 1,
            threadKey: key,
          })
        }
      }
    }
    return out
  })

  /** Short label for the per-row account chip in unified mode. We
      prefer the display name and fall back to the email's local part
      so the chip stays compact even with long names. */
  function accountLabel(id: string): string {
    const a = accounts.find((x) => x.id === id)
    if (!a) return ''
    if (a.display_name) return a.display_name
    return a.email.split('@')[0] ?? a.email
  }

  // ── Fetch state ─────────────────────────────────────────────
  //
  // Two-phase load: first ask the cache (instant, offline-safe), then
  // kick off the network refresh in parallel. `loading` covers the
  // *initial* paint and is dropped as soon as either source returns.
  // `refreshing` stays true while the network call is still in flight
  // after the cache has rendered, so the UI can show a subtle hint
  // without blanking the list.
  let loading = $state(true)
  let error = $state('')

  // ── Multi-select (#89, follow-up) ─────────────────────────────
  // Ctrl/Cmd+clicking rows toggles them in this set; plain-clicking
  // any row clears the set and falls through to the existing
  // single-row select (`onselect`).  The set persists across the
  // session as long as the folder + account stay the same — we
  // clear it whenever the inputs change so a leftover selection
  // never leaks across folders.
  let multiSelectedUids = $state<Set<number>>(new Set())

  $effect(() => {
    // Tracked so the effect re-runs on context change.
    void accountId
    void folder
    void unified
    multiSelectedUids = new Set()
  })

  function isMulti(uid: number): boolean {
    return multiSelectedUids.has(uid)
  }

  function onRowClick(e: MouseEvent, env: EmailEnvelope) {
    if (e.ctrlKey || e.metaKey) {
      // Ctrl/Cmd+click → toggle in multi-select; never opens the row.
      // First ctrl-click on a fresh state promotes the currently-
      // open row into the set so the user's "selection" mental
      // model matches the standard mail-client behaviour: plain-
      // click A, ctrl-click B, drag → both A and B move.  Without
      // this promotion
      // A wasn't in the multi-set and got left behind on every
      // batch operation, which is what looked like "the last
      // selected mail won't move".
      const next = new Set(multiSelectedUids)
      if (next.size === 0 && selectedUid != null && selectedUid !== env.uid) {
        next.add(selectedUid)
      }
      if (next.has(env.uid)) next.delete(env.uid)
      else next.add(env.uid)
      multiSelectedUids = next
      return
    }
    // Plain click — clear multi-select and open as before.
    if (multiSelectedUids.size > 0) multiSelectedUids = new Set()
    onselect(env.uid, unified ? env.account_id : undefined)
  }

  /** Resolve the right (accountId, folder) tuple for a given
   *  envelope.  In unified mode the row carries its owning account
   *  on `env.account_id`; outside unified mode the active account +
   *  folder props are the truth.  Used by drag, right-click move,
   *  and the picker callback. */
  function srcCoordinates(env: EmailEnvelope) {
    return {
      accountId: unified && env.account_id ? env.account_id : accountId,
      folder: env.folder || folder,
    }
  }

  /** Envelopes that should be acted on for an operation triggered
   *  on `env`.  When `env` is part of a multi-select group with
   *  more than one row, we operate on the whole group; otherwise
   *  it's just `env` (this matches the standard mail-client
   *  right-click + drag behaviour). */
  function affectedEnvelopes(env: EmailEnvelope): EmailEnvelope[] {
    if (multiSelectedUids.size > 1 && multiSelectedUids.has(env.uid)) {
      return envelopes.filter((e) => multiSelectedUids.has(e.uid))
    }
    return [env]
  }

  // ── Move-to-folder picker (#89) — opened via the right-click
  // "Move to folder…" entry.  We hold the envelope group being
  // moved here so the picker can target the right account even in
  // unified mode, and so `move_message` gets the correct source
  // `folder` field for each envelope.
  let movingGroup = $state<EmailEnvelope[] | null>(null)

  async function moveGroupToFolder(group: EmailEnvelope[], dest: string) {
    movingGroup = null
    // Group by (accountId, sourceFolder) so each subgroup goes
    // through a single batched IMAP MOVE on the backend
    // (`move_messages`).  Looping per-UID with `move_message` opened
    // a fresh IMAP connection every time and some servers were
    // dropping the last move in the burst due to rapid connection
    // recycling — the batched command does the whole subgroup in
    // one COPY + STORE + EXPUNGE round-trip.
    const groups = new Map<
      string,
      { accountId: string; folder: string; uids: number[] }
    >()
    for (const env of group) {
      const { accountId: src, folder: srcFolder } = srcCoordinates(env)
      if (dest === srcFolder) continue
      const key = `${src} ${srcFolder}`
      const existing = groups.get(key)
      if (existing) existing.uids.push(env.uid)
      else groups.set(key, { accountId: src, folder: srcFolder, uids: [env.uid] })
    }

    // Optimistic UI (#174): snapshot the envelope set being moved
    // for restore-on-failure, then notify the parent for each
    // moved UID FIRST so its auto-advance fires against the
    // still-populated list.  The parent's
    // `App.onMessageRemoved` splices `mailListEnvelopes` (which
    // is bound back to our local `envelopes`), so the local
    // list updates without a separate splice here.  Each
    // backend call's `move_messages` IPC also tombstones the
    // matching cache rows so a folder switch mid-flight doesn't
    // briefly resurrect them.
    const movedUidSet = new Set(group.map((e) => `${e.account_id}::${e.folder}::${e.uid}`))
    const removedSnapshot: { env: EmailEnvelope; idx: number }[] = []
    envelopes.forEach((e, i) => {
      const key = `${e.account_id}::${e.folder}::${e.uid}`
      if (movedUidSet.has(key)) {
        removedSnapshot.push({ env: e, idx: i })
      }
    })
    for (const { env: e } of removedSnapshot) {
      onmessagemoved?.(e.uid)
    }

    const succeeded: number[] = []
    const failures: { uids: number[]; err: unknown }[] = []
    for (const g of groups.values()) {
      try {
        const moved = await invoke<number[]>('move_messages', {
          accountId: g.accountId,
          folder: g.folder,
          uids: g.uids,
          destFolder: dest,
        })
        succeeded.push(...moved)
      } catch (err) {
        console.warn('move_messages failed', err)
        failures.push({ uids: g.uids, err })
      }
    }
    if (failures.length > 0) {
      // Re-insert any envelopes whose subgroup failed.  We rebuild
      // the list rather than splice-by-index because successful
      // moves in earlier subgroups have already shifted indexes.
      const failedUids = new Set(failures.flatMap((f) => f.uids))
      const restore = removedSnapshot
        .filter((r) => failedUids.has(r.env.uid))
        .map((r) => r.env)
      // Keep the user's date-sorted order — easier to merge than
      // try to re-establish exact original indexes against the
      // mutated list.
      const merged = [...envelopes, ...restore].sort(
        (a, b) => +new Date(b.date) - +new Date(a.date),
      )
      envelopes = merged
      error =
        succeeded.length === 0
          ? formatError(failures[0].err) || 'Failed to move message'
          : `Moved ${succeeded.length} of ${group.length} messages — ${failures.length} group(s) failed.`
    }
    multiSelectedUids = new Set()
  }

  // ── Drag source: serialize a list of {accountId, folder, uid}
  // into the dataTransfer payload so Sidebar's folder rows (#89)
  // can iterate moves on drop.  The payload is always an array —
  // single-row drags become a 1-element list.  When the dragged
  // row is part of a multi-select group, the whole group rides
  // along.  The custom `application/x-nimbus-mail` MIME type means
  // the browser ignores the drag for non-Sidebar drop targets.
  function onMailDragStart(e: DragEvent, env: EmailEnvelope) {
    if (!e.dataTransfer) return
    // Dragging a row that *isn't* part of an existing multi-select
    // shouldn't drag the multi-select set — that would surprise the
    // user.  The affectedEnvelopes() rule already does the right
    // thing: it only expands to the group when the dragged row is
    // a member.
    const group = affectedEnvelopes(env)
    const payload = group.map((g) => {
      const { accountId: src, folder: srcFolder } = srcCoordinates(g)
      return { accountId: src, folder: srcFolder, uid: g.uid }
    })
    e.dataTransfer.setData(
      'application/x-nimbus-mail',
      JSON.stringify(payload),
    )
    e.dataTransfer.effectAllowed = 'move'

    // Multi-drag preview: clone the row that triggered the drag
    // and stamp a "[+] N" badge in its bottom-right corner so the
    // user can see how many messages are moving.  Single-drag
    // keeps the default browser drag image — there's no count to
    // show and the row's own visual already conveys what's
    // moving.  The clone is appended offscreen and scheduled for
    // removal after the next frame (setDragImage needs the node
    // to be in the live DOM at the moment the browser snapshots
    // it; removing immediately would beat that snapshot).
    if (group.length <= 1) return
    const rowEl = e.currentTarget as HTMLElement | null
    if (!rowEl) return
    // The drag image — a clone of the row at full opacity.  No
    // badge inside the bitmap, because the OS rendering applies
    // its own opacity to whatever's in the drag image and that
    // would also fade the badge.  Instead the badge floats as a
    // separate live DOM element pinned to the bottom-right
    // corner of the drag image (see `attachFloatingBadge`) and
    // stays at 100% opacity.
    const rect = rowEl.getBoundingClientRect()
    const preview = buildRowDragImage(rowEl)
    const dragAnchor = 16 // matches the (16, 16) offset passed to setDragImage
    if (preview) {
      e.dataTransfer.setDragImage(preview, dragAnchor, dragAnchor)
    }
    attachFloatingBadge(group.length, rect.width, rect.height, dragAnchor)
  }

  /** Clone the source row off-screen at full opacity for use as
   *  the OS-level drag image.  No badge — the badge is rendered
   *  separately, see `attachFloatingBadge`. */
  function buildRowDragImage(rowEl: HTMLElement): HTMLElement | null {
    try {
      const rect = rowEl.getBoundingClientRect()
      const wrapper = document.createElement('div')
      wrapper.style.position = 'fixed'
      wrapper.style.top = '-9999px'
      wrapper.style.left = '-9999px'
      wrapper.style.width = `${rect.width}px`
      wrapper.style.pointerEvents = 'none'
      wrapper.style.boxShadow = '0 8px 20px rgb(0 0 0 / 0.18)'
      wrapper.style.borderRadius = '6px'
      wrapper.style.overflow = 'hidden'
      const clone = rowEl.cloneNode(true) as HTMLElement
      clone.style.width = `${rect.width}px`
      clone.style.background = getComputedStyle(rowEl).backgroundColor
      wrapper.appendChild(clone)
      document.body.appendChild(wrapper)
      // Tear down after the browser has snapshotted the bitmap.
      // `setTimeout(..., 0)` lands the cleanup on the next
      // macrotask, after dragstart's microtask checkpoint where
      // Edge WebView2 takes its snapshot.
      setTimeout(() => wrapper.remove(), 0)
      return wrapper
    } catch (e) {
      console.warn('drag-image clone failed:', e)
      return null
    }
  }

  /** Float a "[+] N" badge that tracks the cursor during a
   *  multi-drag.  Lives in the live DOM (not in the OS drag
   *  bitmap) so the OS' uniform drag-image opacity doesn't
   *  affect it — the badge renders at 100% opacity throughout
   *  the drag.  We listen for `dragover` on the document to
   *  update the badge's position and tear down on `dragend` /
   *  `drop`.  Idempotent: a previous badge from an aborted
   *  drag is removed before we attach a new one. */
  let floatingBadgeEl: HTMLElement | null = null
  let floatingBadgeCleanup: (() => void) | null = null
  function attachFloatingBadge(
    count: number,
    rowWidth: number,
    rowHeight: number,
    dragAnchor: number,
  ) {
    detachFloatingBadge()
    const themed = getComputedStyle(document.documentElement)
      .getPropertyValue('--color-primary-500')
      .trim()
    const accent = themed || '#3b82f6'

    const wrap = document.createElement('div')
    wrap.style.position = 'fixed'
    wrap.style.zIndex = '999999'
    wrap.style.pointerEvents = 'none'
    wrap.style.display = 'inline-flex'
    wrap.style.alignItems = 'center'
    wrap.style.gap = '4px'
    // Position at -9999 initially so the badge doesn't flash at
    // (0,0) in the upper-left between dragstart and the first
    // dragover event.
    wrap.style.top = '-9999px'
    wrap.style.left = '-9999px'

    const circle = document.createElement('span')
    circle.style.width = '20px'
    circle.style.height = '20px'
    circle.style.borderRadius = '999px'
    circle.style.display = 'inline-flex'
    circle.style.alignItems = 'center'
    circle.style.justifyContent = 'center'
    circle.style.background = accent
    circle.style.boxShadow = '0 2px 6px rgb(0 0 0 / 0.25)'
    circle.style.opacity = '0.8'
    // Render the `+` as inline SVG instead of a text glyph.
    // Text-rendered `+` characters carry the host font's
    // baseline / cap-height bias and end up visually off-centre
    // inside the circle even with flex alignment.  An SVG with
    // explicitly-positioned strokes lands perfectly centred
    // regardless of font.
    circle.innerHTML =
      '<svg width="10" height="10" viewBox="0 0 12 12" fill="none" stroke="white" stroke-width="2.5" stroke-linecap="round" aria-hidden="true">' +
      '<line x1="6" y1="2" x2="6" y2="10"/>' +
      '<line x1="2" y1="6" x2="10" y2="6"/>' +
      '</svg>'

    const num = document.createElement('span')
    num.style.fontWeight = '700'
    num.style.fontSize = '12px'
    num.style.padding = '1px 6px'
    num.style.borderRadius = '999px'
    num.style.background = 'white'
    num.style.color = accent
    num.style.boxShadow = '0 2px 6px rgb(0 0 0 / 0.25)'
    num.style.opacity = '0.8'
    num.textContent = String(count)

    wrap.appendChild(circle)
    wrap.appendChild(num)
    document.body.appendChild(wrap)
    floatingBadgeEl = wrap

    // Capture the badge's intrinsic dimensions once it's been
    // appended to the live DOM — needed below to anchor its
    // bottom-right edge precisely at the drag image's
    // bottom-right corner.  Falls back to a sensible default if
    // the layout query somehow returns 0 (it shouldn't, but the
    // pin should still produce a non-broken result).
    const badgeRect = wrap.getBoundingClientRect()
    const badgeW = badgeRect.width || 56
    const badgeH = badgeRect.height || 22

    // Pin the badge to the bottom-right of the drag image.  The
    // OS draws the drag image with its top-left at
    //   `cursor - (dragAnchor, dragAnchor)`,
    // so its bottom-right corner sits at
    //   `cursor + (rowWidth - dragAnchor, rowHeight - dragAnchor)`.
    // We then offset the badge so its OWN bottom-right edge
    // lands there (with a 6px inset so the badge doesn't
    // overhang the row clone's rounded corner).
    const inset = 6
    const onDocDragOver = (ev: DragEvent) => {
      if (!floatingBadgeEl) return
      // Some engines fire dragover with (0, 0) coordinates when
      // the cursor leaves the window briefly; ignore those so
      // the badge doesn't snap to the corner.
      if (ev.clientX === 0 && ev.clientY === 0) return
      const dragImageRight = ev.clientX + (rowWidth - dragAnchor)
      const dragImageBottom = ev.clientY + (rowHeight - dragAnchor)
      floatingBadgeEl.style.left = `${dragImageRight - badgeW - inset}px`
      floatingBadgeEl.style.top = `${dragImageBottom - badgeH - inset}px`
    }
    const onEnd = () => detachFloatingBadge()
    document.addEventListener('dragover', onDocDragOver, true)
    document.addEventListener('dragend', onEnd, true)
    document.addEventListener('drop', onEnd, true)
    floatingBadgeCleanup = () => {
      document.removeEventListener('dragover', onDocDragOver, true)
      document.removeEventListener('dragend', onEnd, true)
      document.removeEventListener('drop', onEnd, true)
    }
  }
  function detachFloatingBadge() {
    if (floatingBadgeEl) {
      floatingBadgeEl.remove()
      floatingBadgeEl = null
    }
    if (floatingBadgeCleanup) {
      floatingBadgeCleanup()
      floatingBadgeCleanup = null
    }
  }

  // Infinite-scroll pagination state (#194). Each (account, folder)
  // pair has its own "older fetch" lifecycle: a flag to prevent
  // double-loads while a request is in flight, and an "exhausted"
  // flag set once the IMAP server returns nothing older — that's
  // the signal to stop trying.
  const PAGE_SIZE = 50
  let loadingOlder = $state(false)
  let olderExhausted = $state(false)
  let scrollContainer = $state<HTMLDivElement | null>(null)

  /** Snapshot of the (account, folder, unified) tuple the last
   *  time the load effect ran.  Used to distinguish "folder /
   *  account changed" (envelopes from the previous view must be
   *  cleared so they don't leak into the new folder) from
   *  "refreshToken bumped while folder unchanged" (envelopes
   *  must be PRESERVED so any older pages the user paginated to
   *  via infinite scroll survive — the merge path in `load()`
   *  handles that case). Without this distinction the previous
   *  fix to the refresh-clobber bug also caused mails from one
   *  folder to leak into the next on folder switch. */
  let lastFolderKey = $state('')

  // Re-fetch whenever the account, folder, unified flag, or
  // refreshToken changes.  Resets the pagination flags every
  // round and clears the rendered envelope list IF the
  // (account, folder, unified) tuple changed — the merge path
  // in `load()` only does the right thing within a single
  // folder.
  $effect(() => {
    refreshToken
    const key = `${unified ? '__all__' : accountId}::${folder}`
    if (key !== lastFolderKey) {
      envelopes = []
      lastFolderKey = key
    }
    loadingOlder = false
    olderExhausted = false
    void load(accountId, folder, unified)
  })

  /** Merge a fresh batch of envelopes (newest N from the cache or
   *  the server) into the current rendered list.  Crucial for
   *  preserving infinite-scroll state (#194 follow-up): the old
   *  "envelopes = fresh" pattern wiped out any older pages the
   *  user had scrolled to whenever `refreshToken` bumped (clicking
   *  the IconRail avatar, marking read, etc.), which collapsed the
   *  list back to 50 rows and reset scroll position to wherever
   *  it could no longer reach.
   *
   *  Strategy: dedupe by `(account_id, uid)`. Fresh entries win on
   *  collision so flag changes (read/starred/etc.) from the
   *  refresh propagate. Older paginated entries the fresh batch
   *  doesn't touch stay in place. Result is sorted newest-first by
   *  date so a freshly-arrived envelope appears at the top.
   *
   *  Trade-off: if a message was expunged server-side between
   *  paginated load and refresh, it stays stale in the UI until
   *  the user switches folders. Acceptable — far better than the
   *  alternative of losing pagination on every keystroke. */
  function mergeEnvelopes(
    existing: EmailEnvelope[],
    fresh: EmailEnvelope[],
  ): EmailEnvelope[] {
    if (existing.length === 0) return fresh
    const byKey = new Map<string, EmailEnvelope>()
    for (const e of existing) byKey.set(`${e.account_id}:${e.uid}`, e)
    for (const e of fresh) byKey.set(`${e.account_id}:${e.uid}`, e) // fresh wins
    const merged = Array.from(byKey.values())
    merged.sort((a, b) => b.date.localeCompare(a.date))
    return merged
  }

  async function load(id: string, f: string, isUnified: boolean) {
    loading = true
    refreshing = false
    error = ''

    // Stale-response guard helper — `id`, `f`, and `isUnified` close
    // over the call's arguments while `accountId`/`folder`/`unified`
    // refer to whatever the parent currently has.
    const stillCurrent = () =>
      isUnified === unified && (isUnified || (id === accountId && f === folder))

    // Cache first — usually instant, may return [] on cold start.
    try {
      const cached = await invoke<EmailEnvelope[]>(
        isUnified ? 'get_unified_cached_envelopes' : 'get_cached_envelopes',
        isUnified
          ? { folder: f, limit: PAGE_SIZE }
          : { accountId: id, folder: f, limit: PAGE_SIZE },
      )
      if (stillCurrent()) {
        envelopes = mergeEnvelopes(envelopes, cached)
        if (envelopes.length > 0) loading = false
      }
    } catch (e: any) {
      // Cache miss is not an error — just ignore and wait for network.
      console.warn('get_cached_envelopes failed:', e)
    }

    // Network refresh. Always runs, even when the cache hit, so users
    // see new mail as soon as the server responds.
    refreshing = envelopes.length > 0
    try {
      const fresh = await invoke<EmailEnvelope[]>(
        isUnified ? 'fetch_unified_envelopes' : 'fetch_envelopes',
        isUnified
          ? { folder: f, limit: PAGE_SIZE }
          : { accountId: id, folder: f, limit: PAGE_SIZE },
      )
      if (stillCurrent()) {
        envelopes = mergeEnvelopes(envelopes, fresh)
      }
    } catch (e: any) {
      if (envelopes.length === 0) {
        error = formatError(e) || 'Failed to load mail'
      } else {
        console.warn('fetch_envelopes failed (showing cached):', e)
      }
    } finally {
      loading = false
      refreshing = false
    }
  }

  /** Compute the smallest UID per account in the currently-rendered
   *  envelope list — the anchor for the next "load older" round.
   *  Returned as a Map<accountId, smallestUid> for the unified mode,
   *  or as a single number for single-account mode. */
  function smallestUidPerAccount(): Map<string, number> {
    const out = new Map<string, number>()
    for (const e of envelopes) {
      const prev = out.get(e.account_id)
      if (prev === undefined || e.uid < prev) {
        out.set(e.account_id, e.uid)
      }
    }
    return out
  }

  /** Fetch the next page of older envelopes via the
   *  `fetch_older_envelopes` Tauri command (#194). Appends to
   *  `envelopes` and persists in cache server-side. Triggered by
   *  the scroll-near-bottom handler below; also safe to call
   *  manually from a "Load older" button if we ever add one. */
  async function loadOlder() {
    if (loadingOlder || olderExhausted || envelopes.length === 0) return
    if (loading) return  // initial paint still in flight

    const idAtCall = accountId
    const folderAtCall = folder
    const unifiedAtCall = unified
    loadingOlder = true
    try {
      let older: EmailEnvelope[]
      if (unifiedAtCall) {
        const map = smallestUidPerAccount()
        if (map.size === 0) {
          olderExhausted = true
          return
        }
        // Tauri serialises Map → JSON object via Object.fromEntries.
        const beforeUidPerAccount: Record<string, number> = {}
        for (const [k, v] of map) beforeUidPerAccount[k] = v
        older = await invoke<EmailEnvelope[]>('fetch_older_unified_envelopes', {
          folder: folderAtCall,
          beforeUidPerAccount,
          limit: PAGE_SIZE,
        })
      } else {
        const smallest = envelopes.reduce<number | null>(
          (acc, e) => (acc === null || e.uid < acc ? e.uid : acc),
          null,
        )
        if (smallest === null) {
          olderExhausted = true
          return
        }
        older = await invoke<EmailEnvelope[]>('fetch_older_envelopes', {
          accountId: idAtCall,
          folder: folderAtCall,
          beforeUid: smallest,
          limit: PAGE_SIZE,
        })
      }

      // Stale-response guard — same shape as `load`.
      const stillCurrent =
        unifiedAtCall === unified
        && (unifiedAtCall || (idAtCall === accountId && folderAtCall === folder))
      if (!stillCurrent) return

      if (older.length === 0) {
        olderExhausted = true
        return
      }

      // De-dupe in case the server includes a UID we already have
      // (UID-search overlaps are rare but possible if a poll arrives
      // mid-pagination). Newest-first ordering preserved by sorting
      // the merged list by date descending.
      const seen = new Set(envelopes.map((e) => `${e.account_id}:${e.uid}`))
      const fresh = older.filter((e) => !seen.has(`${e.account_id}:${e.uid}`))
      const merged = [...envelopes, ...fresh]
      merged.sort((a, b) => b.date.localeCompare(a.date))
      envelopes = merged

      // If the server returned fewer than we asked for, there's
      // probably nothing left — stop asking. (A folder with
      // exactly PAGE_SIZE older messages will trigger one extra
      // empty round, which is fine.)
      if (older.length < PAGE_SIZE) olderExhausted = true
    } catch (e) {
      console.warn('fetch_older_envelopes failed:', e)
    } finally {
      loadingOlder = false
    }
  }

  /** How far above the bottom we trigger the next "load older"
   *  round.  Generous (~2 viewports' worth of buffer) so the
   *  network round-trip lands well before the user actually
   *  scrolls into the unloaded region — they never see the
   *  spinner unless they're scrolling at hard-flick speed. */
  const PAGER_PREFETCH_PX = 1500

  /** Scroll handler — fires the next "load older" round as soon
   *  as the user is within `PAGER_PREFETCH_PX` of the bottom. */
  function onListScroll(e: Event) {
    const el = e.currentTarget as HTMLDivElement
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    if (distanceFromBottom < PAGER_PREFETCH_PX) {
      void loadOlder()
    }
  }

  /** Eager prefetch (#194 follow-up): as soon as the initial
   *  load lands, kick off the next page in the background so
   *  the user can scroll past the first 50 rows without ever
   *  hitting a "Loading older messages…" pause. Re-fires when
   *  the folder / account / unified flag changes — each fresh
   *  open prefetches its own next page. The flag below stops
   *  it from looping past the first prefetch on any given
   *  folder; subsequent pages still come via the scroll-based
   *  trigger. */
  let prefetchedFor = $state<string | null>(null)
  $effect(() => {
    const key = `${unified ? '__all__' : accountId}::${folder}`
    if (prefetchedFor === key) return
    if (loading || loadingOlder) return
    if (envelopes.length === 0) return
    if (olderExhausted) return
    prefetchedFor = key
    void loadOlder()
  })

  // ── Answered-indicator (#255) ───────────────────────────────
  // Small icon prefixed to the subject when this message has
  // been answered.  Three sources of truth, in priority order:
  //
  //   1. `replied_kind` — Nimbus stamped this when the user
  //      replied via Compose.  Carries the *kind* of reply
  //      (reply / reply-all / meeting), so we can pick the
  //      matching icon.
  //   2. `is_answered` — the IMAP `\Answered` system flag.
  //      True when *anyone* (Nimbus, another mail client, the
  //      user's phone) has answered the message.  We don't
  //      know how, so fall back to the generic reply icon.
  //   3. neither — return null, the subject renders without an
  //      icon prefix.
  function answeredIconName(
    env: EmailEnvelope,
  ): 'reply' | 'reply-all' | 'respond-with-meeting' | null {
    switch (env.replied_kind) {
      case 'reply':
        return 'reply'
      case 'reply-all':
        return 'reply-all'
      case 'meeting':
        return 'respond-with-meeting'
    }
    return env.is_answered ? 'reply' : null
  }
  function answeredIconTitle(env: EmailEnvelope): string {
    switch (env.replied_kind) {
      case 'reply':
        return 'You replied'
      case 'reply-all':
        return 'You replied to all'
      case 'meeting':
        return 'You responded with a meeting'
    }
    return 'Answered'
  }

  // Render dates compactly: today → time, otherwise short date.
  function formatDate(iso: string): string {
    const d = new Date(iso)
    const now = new Date()
    const sameDay = d.toDateString() === now.toDateString()
    if (sameDay) {
      return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    }
    return d.toLocaleDateString([], { month: 'short', day: 'numeric' })
  }

  // ── Right-click context menu ──────────────────────────────────
  // Positioned absolutely at the click coordinates. Closing it is
  // delegated to a window-level click / keydown listener so any
  // interaction outside the menu dismisses it (no overlay element
  // needed). Menu actions act on the captured `env`, not on whatever
  // is currently selected — right-clicking row B while row A is open
  // should affect row B.
  let contextMenu = $state<{
    x: number
    y: number
    env: EmailEnvelope
  } | null>(null)

  function openContextMenu(e: MouseEvent, env: EmailEnvelope) {
    e.preventDefault()
    contextMenu = { x: e.clientX, y: e.clientY, env }
  }

  function closeContextMenu() {
    contextMenu = null
  }

  $effect(() => {
    if (!contextMenu) return
    const onDocClick = () => closeContextMenu()
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeContextMenu()
    }
    // `setTimeout` so the click that *opened* the menu doesn't also
    // close it on the same frame.
    const t = setTimeout(() => {
      window.addEventListener('click', onDocClick)
      window.addEventListener('keydown', onKey)
    }, 0)
    return () => {
      clearTimeout(t)
      window.removeEventListener('click', onDocClick)
      window.removeEventListener('keydown', onKey)
    }
  })

  /** Toggle a single envelope's read state from the context menu.
      Optimistic: flip the local row immediately so the bold styling
      updates without a round-trip, then call the backend. The
      backend in turn fires `unread-count-updated`, which the Sidebar
      listener uses to refresh the per-folder badge. */
  // ── Quick-action handlers (#98) ───────────────────────────────
  // Inline icon buttons on each mail-list row — Delete + Mark
  // read/unread + Move to folder — so the user can triage a stack
  // of mail without ever opening it.  All three follow the same
  // optimistic shape: the row visibly disappears / changes state
  // instantly, the backend call follows, errors get surfaced into
  // the existing error banner.
  //
  // Click handlers MUST `stopPropagation` so the row-level click /
  // dblclick / dragstart never fires alongside the action.

  async function quickDelete(env: EmailEnvelope) {
    const srcAccountId = env.account_id || accountId
    const srcFolder = env.folder || folder
    // Optimistic: notify the parent FIRST so its auto-advance
    // (`App.onMessageRemoved`) can find the next neighbour
    // against the still-populated list, then splice the row out
    // via the bound `mailListEnvelopes` mirror.  Doing the
    // local splice before `onmessagemoved` left the parent
    // unable to find the removed row by uid (it was already
    // gone), so `selectedUid` defaulted to `null` and the
    // reading pane went blank instead of advancing (#174 bug).
    const idx = envelopes.findIndex(
      (e) => e.uid === env.uid && e.folder === env.folder && e.account_id === env.account_id,
    )
    const removed = idx >= 0 ? envelopes[idx] : null
    onmessagemoved?.(env.uid)
    try {
      await invoke('delete_message', {
        accountId: srcAccountId,
        folder: srcFolder,
        uid: env.uid,
      })
    } catch (err) {
      console.warn('quickDelete failed', err)
      error = formatError(err) || 'Failed to delete'
      if (removed && idx >= 0) {
        envelopes = [...envelopes.slice(0, idx), removed, ...envelopes.slice(idx)]
      }
    }
  }

  function quickMove(env: EmailEnvelope) {
    // Re-uses the multi-select "group" picker plumbing — a single-row
    // quick-action move is just a 1-element group from its
    // perspective.  affectedEnvelopes does the right thing here:
    // when this row is part of a multi-select group, the picker
    // moves the whole group; otherwise just the one row.
    movingGroup = affectedEnvelopes(env)
  }

  async function toggleEnvelopeRead(env: EmailEnvelope) {
    const next = !env.is_read
    env.is_read = next
    closeContextMenu()
    try {
      await invoke('set_message_read', {
        accountId: env.account_id || accountId,
        folder: env.folder,
        uid: env.uid,
        read: next,
      })
    } catch (e) {
      console.warn('set_message_read failed:', e)
      env.is_read = !next
    }
  }
</script>

<div class="flex-1 flex flex-col min-w-0">

  <!-- Email list -->
  <div
    class="flex-1 overflow-y-auto"
    bind:this={scrollContainer}
    onscroll={onListScroll}
  >
    {#if loading}
      <div class="p-6 text-center text-sm text-surface-500">Loading…</div>
    {:else if error}
      <div class="p-4 text-sm text-red-500">{error}</div>
    {:else if envelopes.length === 0}
      <div class="p-6 text-center text-sm text-surface-500">No messages in {folder}.</div>
    {:else}
      {#each renderRows as row (`${row.env.account_id}:${row.env.uid}:${row.isSibling ? 's' : 'h'}`)}
        {@const env = row.env}
        {@const selected = selectedUid === env.uid && (!unified || selectedUid === env.uid)}
        {@const multi = isMulti(env.uid)}
        <!-- Unread visual treatment: a 3px themed accent strip on the
             leading edge plus a subtle primary tint on the row.  The
             border is always present (transparent when read) so rows
             never reflow between states.  Selection > multi-select >
             unread tint for the background colour; the accent strip
             stays orthogonal so an unread+selected row keeps both.
             The row is wrapped in a `group` so the inline quick-
             action icons (#98) reveal on row hover.
             Sibling rows of an expanded thread (#277) get an
             absolutely-positioned thread connector — a vertical
             dotted line spanning the row's full height plus a
             solid dot at its midpoint.  When multiple siblings
             stack the dotted lines join into one continuous
             vertical track, with one dot per child anchored to
             the line.  The inner row content shifts right
             (`pl-12`) to clear the connector; the from / subject
             columns naturally indent under the head as the user
             expects. -->
        <div class="group relative {row.isSibling ? 'bg-surface-50/50 dark:bg-surface-900/30' : ''}">
          {#if row.isSibling}
            <!-- Vertical dotted track + dot.
                 We render the dots via a background-image
                 (`repeating-linear-gradient`) instead of
                 `border-dotted` because the latter spaces dots
                 adaptively to fit the border length — two
                 stacked rows of different heights produce
                 different phases, and the line visibly jogs at
                 every row boundary.  Fixed pixel pattern (2 px
                 dot, 3 px gap, 5 px cycle) keeps the spacing
                 identical regardless of row height.
                 The element is `w-0.5` (2 px) and uses
                 `-translate-x-1/2` to centre at `left-6`, the
                 same x as the dot so the dot sits *on* the
                 line.  `text-primary-500/60` sets the colour
                 the gradient picks up via `currentColor`. -->
            <span
              class="pointer-events-none absolute left-6 top-0 w-0.5 -translate-x-1/2 text-primary-500/60 {row.isLastSibling ? 'bottom-1/2' : 'bottom-0'}"
              style="background-image: repeating-linear-gradient(to bottom, currentColor 0 2px, transparent 2px 5px);"
              aria-hidden="true"
            ></span>
            <span
              class="pointer-events-none absolute left-6 top-1/2 w-2 h-2 -translate-x-1/2 -translate-y-1/2 rounded-full bg-primary-500"
              aria-hidden="true"
            ></span>
          {/if}
          <!-- Row is a `<div role="button">` rather than a real
               `<button>` because several webview engines (notably
               Edge WebView2 on Windows) refuse to fire
               `dragstart` on a `<button>` element regardless of
               `draggable="true"` — drag-to-folder silently fails
               for those users.  We replicate the button-shaped
               affordance with role + tabindex + Enter/Space
               keyboard handling so accessibility is preserved.
               (#89 / drag-drop bugfix.) -->
          <div
            role="button"
            tabindex="0"
            aria-pressed={selected}
            class="w-full text-left {row.isSibling ? 'pl-12' : 'pl-3'} pr-4 py-3 border-b border-l-[3px] border-surface-100 dark:border-surface-800 transition-colors cursor-pointer
              {!env.is_read ? 'border-l-primary-500' : 'border-l-transparent'}
              {selected
                ? 'bg-primary-500/10'
                : multi
                  ? 'bg-primary-500/15 hover:bg-primary-500/20'
                  : !env.is_read
                    ? 'bg-primary-500/4 dark:bg-primary-500/7 hover:bg-primary-500/10'
                    : 'hover:bg-surface-100 dark:hover:bg-surface-800'}"
            draggable="true"
            ondragstart={(e) => onMailDragStart(e, env)}
            onclick={(e) => onRowClick(e, env)}
            ondblclick={() =>
              openMailInStandaloneWindow(
                unified && env.account_id ? env.account_id : accountId,
                folder,
                env.uid,
              )}
            onkeydown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault()
                onRowClick(e as unknown as MouseEvent, env)
              }
            }}
            oncontextmenu={(e) => openContextMenu(e, env)}
          >
            <div class="flex items-center justify-between mb-1">
              <span class="text-sm {!env.is_read ? 'font-semibold' : 'font-normal'} truncate pr-2">
                {env.from || '(unknown sender)'}
              </span>
              <span class="text-xs {!env.is_read ? 'text-primary-500 font-medium' : 'text-surface-500'} shrink-0">{formatDate(env.date)}</span>
            </div>
            <p class="text-sm {!env.is_read ? 'font-medium' : ''} truncate flex items-center gap-1.5">
              {#if answeredIconName(env)}
                <span
                  class="shrink-0 inline-flex items-center text-primary-500"
                  title={answeredIconTitle(env)}
                  aria-label={answeredIconTitle(env)}
                >
                  <Icon name={answeredIconName(env)!} size={14} />
                </span>
              {/if}
              <span class="truncate min-w-0">
                {env.subject || '(no subject)'}
              </span>
            </p>
            <!-- Bottom meta row.  Conversation count + chevron
                 (#277) sits at the bottom-left as a pill badge;
                 the unified-mode account label, when present,
                 trails to the right via `ml-auto`.  Only renders
                 if at least one piece has content; otherwise the
                 row stays compact. -->
            {#if row.siblingCount > 0 || (unified && env.account_id)}
              <div class="flex items-center gap-2 mt-1 text-[11px] text-surface-500 min-w-0">
                {#if row.siblingCount > 0}
                  <!-- Modern pill badge: rounded-full, soft
                       primary tint, primary-coloured count, and
                       an inline SVG chevron that rotates 180° on
                       expand.  Click toggles the thread below;
                       `stopPropagation` so the row click (which
                       opens the head message) doesn't fire
                       alongside.  Inline SVG instead of an Icon
                       registry entry — `chevron-down` isn't a
                       stock icon in Icon.svelte and a 12 px path
                       is too small to justify a new file. -->
                  <button
                    type="button"
                    class="shrink-0 inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[11px] font-medium bg-primary-500/10 text-primary-600 dark:text-primary-400 hover:bg-primary-500/20 transition-colors"
                    title={expandedThreads.has(row.threadKey)
                      ? 'Collapse conversation'
                      : 'Show full conversation'}
                    onclick={(e) => {
                      e.stopPropagation()
                      toggleThread(row.threadKey)
                    }}
                  >
                    <span>{row.siblingCount + 1}</span>
                    <svg
                      class="w-2.5 h-2.5 transition-transform duration-150 {expandedThreads.has(row.threadKey) ? 'rotate-180' : ''}"
                      viewBox="0 0 16 16"
                      fill="none"
                      stroke="currentColor"
                      stroke-width="2.5"
                      stroke-linecap="round"
                      stroke-linejoin="round"
                      aria-hidden="true"
                    >
                      <path d="M4 6 L8 10 L12 6" />
                    </svg>
                  </button>
                {/if}
                {#if unified && env.account_id}
                  <span class="truncate ml-auto">{accountLabel(env.account_id)}</span>
                {/if}
              </div>
            {/if}
          </div>
          <!-- Hover-revealed quick actions (#98).  Anchored to the
               BOTTOM-right corner of the row so the cluster never
               overlaps the date in the top-right.  Sibling of the
               row.
               `pointer-events-none` on the wrapper while hidden
               keeps the layer click-through so the row's drag /
               click still work in the gap. -->
          <div
            class="absolute right-1 bottom-3 flex items-center gap-0.5 opacity-0 pointer-events-none transition-opacity
                   group-hover:opacity-100 group-hover:pointer-events-auto
                   focus-within:opacity-100 focus-within:pointer-events-auto"
          >
            <button
              type="button"
              class="w-7 h-7 rounded-md flex items-center justify-center text-sm bg-surface-50/90 dark:bg-surface-800/90 hover:bg-surface-200 dark:hover:bg-surface-700 shadow-sm"
              title={env.is_read ? 'Mark as unread' : 'Mark as read'}
              aria-label={env.is_read ? 'Mark as unread' : 'Mark as read'}
              onclick={(e) => {
                e.stopPropagation()
                void toggleEnvelopeRead(env)
              }}
            ><Icon name={env.is_read ? 'unread' : 'read'} size={16} /></button>
            <button
              type="button"
              class="w-7 h-7 rounded-md flex items-center justify-center bg-surface-50/90 dark:bg-surface-800/90 hover:bg-surface-200 dark:hover:bg-surface-700 shadow-sm"
              title="Move to folder"
              aria-label="Move to folder"
              onclick={(e) => {
                e.stopPropagation()
                quickMove(env)
              }}
            ><Icon name="move-to-folder" size={16} /></button>
            <button
              type="button"
              class="w-7 h-7 rounded-md flex items-center justify-center bg-surface-50/90 dark:bg-surface-800/90 hover:bg-red-500/20 hover:text-red-500 shadow-sm"
              title="Delete"
              aria-label="Delete"
              onclick={(e) => {
                e.stopPropagation()
                void quickDelete(env)
              }}
            ><Icon name="trash" size={16} /></button>
          </div>
        </div>
      {/each}

      <!-- Infinite-scroll status row (#194). Sits at the bottom of
           the list to give the user a calm signal of the
           pagination state — a thin loading hint while the next
           page is in flight, a quiet "end of folder" line once
           the IMAP server has told us there's nothing older. The
           scroll handler keeps fetching automatically. -->
      {#if loadingOlder}
        <div class="px-4 py-3 text-center text-xs text-surface-500 inline-flex items-center justify-center gap-2 w-full">
          <Icon name="loading" size={14} />
          Loading older messages…
        </div>
      {:else if olderExhausted && envelopes.length > 0}
        <div class="px-4 py-3 text-center text-[11px] text-surface-400 uppercase tracking-wider">
          End of folder
        </div>
      {/if}
    {/if}
  </div>
</div>

{#if contextMenu}
  {@const ctxGroup = affectedEnvelopes(contextMenu.env)}
  {@const groupSize = ctxGroup.length}
  <!-- Right-click menu. Stop propagation so a click *inside* the menu
       doesn't reach the window-level dismiss listener and close it
       before the action handler runs. `role="menu"` keeps screen
       readers oriented. -->
  <div
    class="fixed z-50 min-w-45 py-1 rounded-md shadow-lg border border-surface-200 dark:border-surface-700 bg-surface-50 dark:bg-surface-900 text-sm"
    style="top: {contextMenu.y}px; left: {contextMenu.x}px;"
    role="menu"
    tabindex="-1"
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.key === 'Escape' && closeContextMenu()}
    oncontextmenu={(e) => e.preventDefault()}
  >
    <button
      type="button"
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800"
      onclick={() => {
        if (!contextMenu) return
        // For a single-row context menu just flip the row's read
        // flag.  For a multi-row group flip every row to the
        // *opposite* of the right-clicked row's current state, so
        // a mixed group converges to one consistent state in one
        // click (the standard mail-client behaviour).
        if (groupSize > 1) {
          const target = !contextMenu.env.is_read
          for (const env of ctxGroup) {
            if (env.is_read !== target) void toggleEnvelopeRead(env)
          }
          multiSelectedUids = new Set()
          closeContextMenu()
        } else {
          void toggleEnvelopeRead(contextMenu.env)
        }
      }}
    >
      <Icon name={contextMenu.env.is_read ? 'unread' : 'read'} size={16} />
      <span>
        {#if groupSize > 1}
          Mark {groupSize} as {contextMenu.env.is_read ? 'unread' : 'read'}
        {:else}
          {contextMenu.env.is_read ? 'Mark as unread' : 'Mark as read'}
        {/if}
      </span>
    </button>
    <button
      type="button"
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800"
      onclick={() => {
        if (!contextMenu) return
        movingGroup = ctxGroup
        closeContextMenu()
      }}
    >
      <Icon name="move-to-folder" size={16} />
      <span>
        {#if groupSize > 1}
          Move {groupSize} messages to folder…
        {:else}
          Move to folder…
        {/if}
      </span>
    </button>
    <div class="my-1 border-t border-surface-200 dark:border-surface-700"></div>
    <button
      type="button"
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-red-500/10 hover:text-red-500"
      onclick={() => {
        if (!contextMenu) return
        // Single-row delete reuses the row-level `quickDelete` (which
        // already feeds through `onmessagemoved` for auto-advance).
        // Multi-row batches iterate the group sequentially —
        // `delete_message` opens its own short-lived IMAP session per
        // call, but unlike MOVE we don't have a batched server-side
        // command yet.  N is small in practice (the user just
        // hand-picked the rows) so the overhead is acceptable.
        if (groupSize > 1) {
          for (const env of ctxGroup) void quickDelete(env)
          multiSelectedUids = new Set()
        } else {
          void quickDelete(contextMenu.env)
        }
        closeContextMenu()
      }}
    >
      <Icon name="delete" size={16} />
      <span>
        {#if groupSize > 1}
          Delete {groupSize} messages
        {:else}
          Delete
        {/if}
      </span>
    </button>
  </div>
{/if}

{#if movingGroup && movingGroup.length > 0}
  {@const head = movingGroup[0]!}
  <MoveFolderPicker
    accountId={unified && head.account_id ? head.account_id : accountId}
    currentFolder={head.folder || folder}
    accounts={accounts}
    onpicked={(name) => {
      const group = movingGroup
      if (group) void moveGroupToFolder(group, name)
    }}
    onclose={() => (movingGroup = null)}
  />
{/if}
