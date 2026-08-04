<script lang="ts" module>
  // Session-scoped preview cache shared across every NcPreview
  // instance.  Keyed by `${accountId}::${path}` and stores either
  // a blob URL (success) or null (no preview available — server
  // 404 or unsupported type), so subsequent mounts of the same
  // file skip the IPC entirely.  Blob URLs are intentionally
  // not revoked: the picker is opened for a few seconds at a
  // time and the URLs become stale on app restart.
  const PREVIEW_CACHE = new Map<string, string | null>()
</script>

<script lang="ts">
  // Inline thumbnail for a file in the Nextcloud picker.  For
  // image and video rows it lazy-fetches the server-rendered
  // preview from `/index.php/core/preview.png?...` via the
  // `nextcloud_file_preview` Tauri command and renders an
  // `<img>` once the bytes land; everything else falls through
  // to <FileTypeIcon>.
  import * as api from './api'
  import FileTypeIcon from './FileTypeIcon.svelte'

  interface Props {
    accountId: string
    path: string
    contentType?: string | null
    filename: string
    /** Square thumbnail size — Tailwind utility (e.g. `w-9 h-9`). */
    class?: string
  }
  let {
    accountId,
    path,
    contentType = '',
    filename,
    class: cls = 'w-9 h-9',
  }: Props = $props()

  function isPreviewable(): boolean {
    // Only types Nextcloud's `core/preview` endpoint reliably
    // serves out of the box: images and (with previewgenerator
    // installed) PDFs.  Video preview generation is opt-in on
    // the server, not the default — and downloading the full
    // file client-side just to extract a poster frame is too
    // much overhead for a 36 px chip.  When the server has no
    // preview, we fall through to the typed icon.
    const ct = (contentType ?? '').toLowerCase()
    if (ct.startsWith('image/')) return true
    const dot = filename.lastIndexOf('.')
    const ext = dot >= 0 ? filename.slice(dot + 1).toLowerCase() : ''
    return [
      'jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'bmp', 'tif', 'tiff', 'heic', 'heif',
    ].includes(ext)
  }

  let previewUrl = $state<string | null>(null)
  $effect(() => {
    if (!isPreviewable()) {
      previewUrl = null
      return
    }
    const key = `${accountId}::${path}`
    if (PREVIEW_CACHE.has(key)) {
      previewUrl = PREVIEW_CACHE.get(key) ?? null
      return
    }
    let cancelled = false
    void (async () => {
      try {
        const bytes = await api.nextcloud.nextcloudFilePreview({
          ncId: accountId,
          path,
          size: 128,
        })
        if (cancelled) return
        if (!bytes || bytes.length === 0) {
          PREVIEW_CACHE.set(key, null)
          return
        }
        // Nextcloud's preview endpoint always re-encodes to PNG
        // regardless of source format, so the MIME we hand to
        // the Blob is fine to hardcode.
        const blob = new Blob([new Uint8Array(bytes)], { type: 'image/png' })
        const url = URL.createObjectURL(blob)
        PREVIEW_CACHE.set(key, url)
        previewUrl = url
      } catch (e) {
        if (!cancelled) {
          console.warn('nextcloud_file_preview failed', e)
          PREVIEW_CACHE.set(key, null)
        }
      }
    })()
    return () => {
      cancelled = true
    }
  })
</script>

{#if previewUrl}
  <img
    src={previewUrl}
    alt=""
    loading="lazy"
    class="{cls} object-cover rounded shrink-0 bg-surface-200 dark:bg-surface-800"
  />
{:else}
  <FileTypeIcon contentType={contentType} filename={filename} class={cls} />
{/if}
