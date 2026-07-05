/**
 * Groupware-source kind helper (#413).
 *
 * `get_nextcloud_accounts` returns every configured source: full
 * Nextcloud connections, generic CardDAV/CalDAV servers, and local
 * on-device stores. Surfaces that use Nextcloud-only features (Talk,
 * Files, Notes, OCS profile lookups, the settings-bundle backup)
 * must filter with `isNextcloudSource`; contact/calendar surfaces
 * work with every kind and should not filter.
 *
 * The `?? 'nextcloud'` default mirrors the backend's serde default:
 * records saved before the `kind` field existed are Nextclouds.
 */
export type DavSourceKind = 'nextcloud' | 'dav' | 'local'

export function isNextcloudSource(a: { kind?: DavSourceKind | string }): boolean {
  return (a.kind ?? 'nextcloud') === 'nextcloud'
}
