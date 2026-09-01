<script lang="ts" module>
  // Cache: video bytes → first-frame data URL.  Two backings so
  // we can be honest about lifetimes:
  //
  // - WeakMap keyed by the bytes reference — used by hosts that
  //   reuse the same array each render (Compose).  When the
  //   Compose surface unmounts and drops the Attachment object,
  //   the bytes array becomes unreferenced and GC reclaims its
  //   cache entry too.  A "send" or "cancel" therefore frees
  //   every cached poster for that draft automatically.
  // - Map keyed by an explicit string id — used when the host
  //   produces fresh byte arrays per mount (MailView fetches
  //   bytes on demand).  Session-scoped so re-opening the same
  //   mail later doesn't re-decode.
  const FRAME_CACHE_REF = new WeakMap<object, string>()
  const FRAME_CACHE_KEY = new Map<string, string>()
  function cacheGet(key: unknown): string | undefined {
    if (typeof key === 'string') return FRAME_CACHE_KEY.get(key)
    if (key && typeof key === 'object') return FRAME_CACHE_REF.get(key as object)
    return undefined
  }
  function cachePut(key: unknown, val: string): void {
    if (typeof key === 'string') FRAME_CACHE_KEY.set(key, val)
    else if (key && typeof key === 'object') FRAME_CACHE_REF.set(key as object, val)
  }

  // Image blob URLs cached so re-mounts (e.g. opening the `/`
  // picker repeatedly) reuse the same Blob URL instead of
  // building a fresh one each time — building a Blob from a
  // multi-MB number[] is an O(n) copy that adds up across a
  // dropdown of attachments.
  const IMAGE_BLOB_REF = new WeakMap<object, string>()
  const IMAGE_BLOB_KEY = new Map<string, string>()
  function imageBlobGet(key: unknown): string | undefined {
    if (typeof key === 'string') return IMAGE_BLOB_KEY.get(key)
    if (key && typeof key === 'object') return IMAGE_BLOB_REF.get(key as object)
    return undefined
  }
  function imageBlobPut(key: unknown, val: string): void {
    if (typeof key === 'string') IMAGE_BLOB_KEY.set(key, val)
    else if (key && typeof key === 'object') IMAGE_BLOB_REF.set(key as object, val)
  }

  function isImageGuess(contentType: string | null | undefined, filename: string): boolean {
    const ct = (contentType ?? '').toLowerCase()
    if (ct.startsWith('image/')) return true
    const dot = filename.lastIndexOf('.')
    const ext = dot >= 0 ? filename.slice(dot + 1).toLowerCase() : ''
    return ['jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'bmp', 'tif', 'tiff', 'heic', 'heif', 'svg'].includes(ext)
  }
  function isVideoGuess(contentType: string | null | undefined, filename: string): boolean {
    const ct = (contentType ?? '').toLowerCase()
    if (ct.startsWith('video/')) return true
    const dot = filename.lastIndexOf('.')
    const ext = dot >= 0 ? filename.slice(dot + 1).toLowerCase() : ''
    return ['mp4', 'mkv', 'mov', 'avi', 'webm', 'm4v', 'mpg', 'mpeg', '3gp', 'wmv', 'flv'].includes(ext)
  }

  /** Pre-warm the image blob URL + video first-frame caches for
   *  an attachment.  Compose calls this the moment an attachment
   *  is added so the editor's `/` picker, which may open
   *  milliseconds later, sees fully-resolved thumbs without
   *  having to mount any preview component.
   *
   *  Image blob URLs are built synchronously (cheap O(n) array
   *  copy) so they're guaranteed available the instant the
   *  picker opens.  Video first-frame extraction is async (a
   *  GStreamer pipeline cycle on Linux WebKit) and runs through
   *  the global extractionChain so the picker may briefly show
   *  the typed icon for a video that's still decoding. */
  export function prewarm(opts: {
    bytes: Uint8Array | number[]
    contentType?: string | null
    filename: string
    cacheKey?: string | null
  }): void {
    const { bytes, contentType = null, filename, cacheKey = null } = opts
    if (!bytes || bytes.length === 0) return
    const key = cacheKey ?? bytes
    if (isImageGuess(contentType, filename)) {
      if (imageBlobGet(key)) return
      let ct = contentType ?? ''
      if (!ct) ct = 'image/png'
      const u8 = new Uint8Array(bytes)
      const url = URL.createObjectURL(new Blob([u8], { type: ct }))
      imageBlobPut(key, url)
    } else if (isVideoGuess(contentType, filename)) {
      if (cacheGet(key)) return
      let ct = contentType ?? ''
      if (!ct) ct = 'video/mp4'
      const u8 = new Uint8Array(bytes)
      const url = URL.createObjectURL(new Blob([u8], { type: ct }))
      extractionChain = extractionChain
        .then(async () => {
          const frame = await extractFirstFrame(url)
          if (frame) cachePut(key, frame)
        })
        .finally(() => URL.revokeObjectURL(url))
    }
  }

  /** Synchronous read of whatever's currently cached for a
   *  given attachment.  Used by hosts that want to render the
   *  thumb inline without paying the cost of mounting an
   *  AttachmentThumb component (the editor's `/` picker
   *  dropdown).  Returns the blob/data URL, or null if not
   *  cached yet — caller falls back to a typed icon. */
  export function thumbUrlSync(opts: {
    bytes?: Uint8Array | number[] | null
    contentType?: string | null
    filename: string
    cacheKey?: string | null
  }): string | null {
    const { bytes = null, cacheKey = null } = opts
    const key = cacheKey ?? bytes ?? null
    if (key === null) return null
    return imageBlobGet(key) ?? cacheGet(key) ?? null
  }

  /** Seed the in-memory thumb cache from a base64-encoded
   *  thumbnail loaded from the on-disk cache (#157).  Builds
   *  a `data:` URL in place rather than allocating a Blob —
   *  saves an O(n) array copy and survives the data URL's
   *  natural session lifetime. */
  export function seedThumbFromBase64(opts: {
    cacheKey: string
    mime: string
    base64: string
  }): void {
    const { cacheKey, mime, base64 } = opts
    if (!cacheKey || !base64) return
    if (imageBlobGet(cacheKey)) return
    const url = `data:${mime || 'image/jpeg'};base64,${base64}`
    imageBlobPut(cacheKey, url)
  }

  // ── On-disk persistence helpers (#157) ─────────────────────
  //
  // Both run off the critical path via requestIdleCallback so
  // the user's render is never blocked by the downsample +
  // base64 + IPC round-trip.

  type PersistTarget = {
    accountId: string
    folder: string
    uid: number
    partId: number
  }

  function dataUrlToBytes(dataUrl: string): { mime: string; bytes: Uint8Array } | null {
    const m = dataUrl.match(/^data:([^;,]+)(?:;[^,]*)?,(.*)$/)
    if (!m) return null
    const mime = m[1]
    const payload = m[2]
    const isB64 = /;base64/.test(dataUrl.slice(0, dataUrl.indexOf(',')))
    let binary: string
    try {
      binary = isB64 ? atob(payload) : decodeURIComponent(payload)
    } catch {
      return null
    }
    const bytes = new Uint8Array(binary.length)
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
    return { mime, bytes }
  }

  function scheduleIdle(cb: () => void): void {
    const w = window as unknown as {
      requestIdleCallback?: (cb: () => void, opts?: { timeout?: number }) => void
    }
    if (typeof w.requestIdleCallback === 'function') {
      w.requestIdleCallback(cb, { timeout: 800 })
    } else {
      setTimeout(cb, 50)
    }
  }

  /** Persist a JPEG/PNG data URL straight to the on-disk
   *  cache (used by the video extractor — its output is
   *  already a small JPEG).  We forward the data URL's
   *  base64 payload verbatim so Tauri's JSON serialiser
   *  doesn't have to inflate the bytes into a number array. */
  export async function persistFromDataUrl(
    target: PersistTarget,
    dataUrl: string,
  ): Promise<void> {
    return new Promise<void>((resolve) => {
      scheduleIdle(async () => {
        try {
          const m = dataUrl.match(/^data:([^;,]+);base64,(.*)$/)
          if (!m) return resolve()
          const mime = m[1]
          const base64 = m[2]
          await api.mail.putAttachmentPreview({
            accountId: target.accountId,
            folder: target.folder,
            uid: target.uid,
            partId: target.partId,
            mime,
            base64,
          })
        } catch (e) {
          console.warn('put_attachment_preview failed', e)
        } finally {
          resolve()
        }
      })
    })
  }

  /** Load an image blob URL, downsample it to ≤256 px on the
   *  long edge, and persist as JPEG.  Used for image
   *  attachments where the rendered <img> uses the full-size
   *  blob URL but the on-disk cache only needs the thumbnail
   *  (saves space and skips the bytesProvider on next open). */
  export async function persistFromBlobUrl(
    target: PersistTarget,
    blobUrl: string,
    _contentType?: string | null,
  ): Promise<void> {
    return new Promise<void>((resolve) => {
      scheduleIdle(() => {
        const img = new Image()
        img.onload = async () => {
          try {
            const MAX = 256
            const scale = Math.min(1, MAX / Math.max(img.width || MAX, img.height || MAX))
            const w = Math.max(1, Math.round((img.width || MAX) * scale))
            const h = Math.max(1, Math.round((img.height || MAX) * scale))
            const canvas = document.createElement('canvas')
            canvas.width = w
            canvas.height = h
            const ctx = canvas.getContext('2d')
            if (!ctx) return resolve()
            ctx.drawImage(img, 0, 0, w, h)
            const dataUrl = canvas.toDataURL('image/jpeg', 0.78)
            await persistFromDataUrl(target, dataUrl)
          } catch (e) {
            console.warn('persistFromBlobUrl failed', e)
          } finally {
            resolve()
          }
        }
        img.onerror = () => resolve()
        img.src = blobUrl
      })
    })
  }
  // Serialise extractions so a folder full of videos doesn't
  // spin up N pipelines simultaneously — Linux WebKit hits a
  // visible stall when more than two or three pipelines are
  // alive at once.
  let extractionChain: Promise<void> = Promise.resolve()

  function extractFirstFrame(blobUrl: string): Promise<string | null> {
    return new Promise<string | null>((resolve) => {
      const v = document.createElement('video')
      v.muted = true
      v.playsInline = true
      v.preload = 'metadata'
      v.src = blobUrl
      let resolved = false
      const finish = (val: string | null) => {
        if (resolved) return
        resolved = true
        try {
          v.removeAttribute('src')
          v.load()
        } catch {
          /* swallow — element is being torn down anyway */
        }
        v.remove()
        resolve(val)
      }
      const draw = () => {
        try {
          // Scale the canvas to a thumbnail-sized output rather
          // than drawing at the source video's full resolution.
          // A 4K clip would otherwise produce a 3840×2160 PNG
          // encode per file — hundreds of ms each, serialised
          // through the extractionChain, which is exactly what
          // made the picker feel slow with video attachments.
          // 192 px on the long edge fits the largest preview
          // we render (NC picker) with room for high-DPI.
          const MAX_DIM = 192
          const sw = v.videoWidth || MAX_DIM
          const sh = v.videoHeight || MAX_DIM
          const scale = Math.min(1, MAX_DIM / Math.max(sw, sh))
          const canvas = document.createElement('canvas')
          canvas.width = Math.max(1, Math.round(sw * scale))
          canvas.height = Math.max(1, Math.round(sh * scale))
          const ctx = canvas.getContext('2d')
          if (!ctx) return finish(null)
          ctx.drawImage(v, 0, 0, canvas.width, canvas.height)
          // JPEG rather than PNG — ~10× smaller, a fraction of
          // the encode cost, and we don't need transparency for
          // a video frame.
          finish(canvas.toDataURL('image/jpeg', 0.78))
        } catch {
          finish(null)
        }
      }
      v.addEventListener('loadeddata', () => {
        try {
          v.currentTime = Math.min(0.1, v.duration || 0.1)
        } catch {
          draw()
        }
      })
      v.addEventListener('seeked', draw)
      v.addEventListener('error', () => finish(null))
      // Hard cap so a stuck decoder doesn't pin the chain.
      setTimeout(() => finish(null), 8000)
    })
  }
</script>

<script lang="ts">
  // Square inline thumbnail for an attachment.  Image: blob URL
  // → <img> directly.  Video: first frame extracted to a data
  // URL once, cached, rendered as <img> — no <video> elements
  // in the live DOM, which keeps the GStreamer cost limited to
  // the one-time extraction.  Everything else falls through to
  // <FileTypeIcon>.
  import FileTypeIcon from './FileTypeIcon.svelte'
  import * as api from './api'

  interface Props {
    /** Bytes the host already has in memory (Compose path).
     *  Mutually exclusive with `bytesProvider`. */
    bytes?: Uint8Array | number[] | null
    /** Async loader for cases where bytes aren't in memory yet
     *  (MailView lazy-fetches via download_email_attachment).
     *  Resolves to the raw bytes, or `null` to fall back to the
     *  typed icon. */
    bytesProvider?: () => Promise<Uint8Array | number[] | null>
    contentType?: string | null
    filename: string
    /** Tailwind sizing — default w-9 h-9. */
    class?: string
    /** Stable key for the video first-frame cache.  When the
     *  host always passes the same `bytes` reference (Compose),
     *  omit it and the bytes ref doubles as the key.  When the
     *  host produces fresh byte arrays each mount (MailView's
     *  `bytesProvider`), pass an account/path-derived string so
     *  re-mounts hit the cache instead of re-decoding. */
    cacheKey?: string | null
    /** When set, the rendered thumbnail is also persisted to
     *  the on-disk cache (`put_attachment_preview` Tauri
     *  command) so subsequent mounts of the same email skip
     *  the bytes fetch + re-extraction.  Only makes sense for
     *  MailView attachments — Compose drafts are ephemeral. */
    persistTo?: { accountId: string; folder: string; uid: number; partId: number } | null
  }
  let {
    bytes = null,
    bytesProvider,
    contentType = '',
    filename,
    class: cls = 'w-9 h-9',
    cacheKey = null,
    persistTo = null,
  }: Props = $props()

  function ext(): string {
    const dot = filename.lastIndexOf('.')
    return dot >= 0 ? filename.slice(dot + 1).toLowerCase() : ''
  }
  function isImage(): boolean {
    const ct = (contentType ?? '').toLowerCase()
    if (ct.startsWith('image/')) return true
    return ['jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'bmp', 'tif', 'tiff', 'heic', 'heif', 'svg'].includes(ext())
  }
  function isVideo(): boolean {
    const ct = (contentType ?? '').toLowerCase()
    if (ct.startsWith('video/')) return true
    return ['mp4', 'mkv', 'mov', 'avi', 'webm', 'm4v', 'mpg', 'mpeg', '3gp', 'wmv', 'flv'].includes(ext())
  }

  /** Source for the rendered <img>.  For images: a blob URL.
   *  For videos: a data URL produced by canvas first-frame
   *  extraction (cached so subsequent mounts skip the decode). */
  let imgUrl = $state<string | null>(null)

  function makeBlobUrl(b: Uint8Array | number[]): string {
    let ct = contentType ?? ''
    if (!ct) ct = isVideo() ? 'video/mp4' : 'image/png'
    const u8 = new Uint8Array(b)
    return URL.createObjectURL(new Blob([u8], { type: ct }))
  }

  function frameKey(b: Uint8Array | number[]): unknown {
    return cacheKey ?? b
  }

  async function loadFrame(b: Uint8Array | number[]): Promise<string | null> {
    const key = frameKey(b)
    const hit = cacheGet(key)
    if (hit) return hit
    const url = makeBlobUrl(b)
    // Chain through the global queue so concurrent mounts don't
    // each instantiate a GStreamer pipeline at the same time.
    let frame: string | null = null
    extractionChain = extractionChain.then(async () => {
      frame = await extractFirstFrame(url)
    })
    await extractionChain
    URL.revokeObjectURL(url)
    if (frame) cachePut(key, frame)
    return frame
  }

  $effect(() => {
    if (!isImage() && !isVideo()) {
      imgUrl = null
      return
    }
    // Fast path: if the cache already has a hit for this
    // attachment, render straight from it and never run
    // bytesProvider.  This is the difference between a cold
    // open of a previously-seen email being IPC-bound vs.
    // instant — the seeded data URL on the cacheKey IS the
    // whole point of #157.
    if (cacheKey) {
      const cached = imageBlobGet(cacheKey) ?? cacheGet(cacheKey)
      if (cached) {
        imgUrl = cached
        return
      }
    }
    let cancelled = false
    const apply = async (b: Uint8Array | number[] | null | undefined) => {
      if (!b || b.length === 0 || cancelled) return
      if (isImage()) {
        const key = cacheKey ?? b
        const cached = imageBlobGet(key)
        if (cached) {
          imgUrl = cached
          return
        }
        const url = makeBlobUrl(b)
        imageBlobPut(key, url)
        imgUrl = url
        // Persist a downsampled JPEG to the on-disk cache so a
        // future mount of this attachment skips the
        // bytesProvider call entirely (#157).
        if (persistTo) void persistFromBlobUrl(persistTo, url, contentType)
      } else if (isVideo()) {
        const frame = await loadFrame(b)
        if (cancelled) return
        imgUrl = frame
        if (frame && persistTo) void persistFromDataUrl(persistTo, frame)
      }
    }
    if (bytes && bytes.length > 0) {
      void apply(bytes)
    } else if (bytesProvider) {
      void (async () => {
        try {
          const b = await bytesProvider()
          if (cancelled) return
          await apply(b)
        } catch (e) {
          console.warn('attachment thumb load failed', e)
        }
      })()
    } else {
      imgUrl = null
    }
    return () => {
      cancelled = true
      // Image blob URLs live in the cache and are reused across
      // mounts; we don't revoke them here.  When the bytes ref
      // becomes unreferenced (Compose unmounts and drops the
      // Attachment), the WeakMap entry is GC'd, which makes the
      // blob URL unreachable and the browser reclaims it.
    }
  })
</script>

{#if imgUrl}
  <img
    src={imgUrl}
    alt=""
    loading="lazy"
    class="{cls} object-cover rounded-sm shrink-0 bg-surface-200 dark:bg-surface-800"
  />
{:else}
  <!-- Fallback while a video frame is extracting (or for non-
       previewable types).  Replaced live by an <img> once the
       extraction chain resolves; cached extractions skip the
       icon and render the image immediately. -->
  <FileTypeIcon contentType={contentType} filename={filename} class={cls} />
{/if}
