/**
 * Placeholder aliases for the backend DTOs referenced by the generated
 * command wrappers (#473).
 *
 * Every alias is `any` for now: the refactor's first goal is compile-
 * checked command names and argument keys, not full payload typing.
 * Tighten these incrementally — replace an alias with a real interface
 * whenever you touch code that consumes it, the same lazy-migration
 * rule the i18n catalogue uses.
 */

/**
 * Backend `unkai_core::models::Account` (typed for real in #534 —
 * the accountsStore extraction touches every consumer). Keys are
 * snake_case: the Rust struct has no serde rename. Fields the Rust
 * side marks `#[serde(default)]` are optional here so setup-wizard
 * call sites can submit partial objects; on the way *out* of the
 * backend they are always present (`Option` fields as `null`).
 */
export interface Account {
  id: string
  display_name: string
  email: string
  imap_host: string
  imap_port: number
  smtp_host: string
  smtp_port: number
  use_jmap?: boolean
  jmap_url?: string | null
  /** Rich-HTML signature (#248); legacy plain text still occurs. */
  signature?: string | null
  /** "Folder name contains X → icon Y" rules for the Sidebar. */
  folder_icons?: FolderIconRule[]
  /** Per-folder icon overrides, full folder path → emoji. */
  folder_icon_overrides?: Record<string, string>
  /** User-trusted TLS leaf certs for this account's servers. */
  trusted_certs?: TrustedCert[]
  /** Optional emoji avatar for the IconRail (#115). */
  emoji?: string | null
  /** Display order in the IconRail; lower = top (#115). */
  sort_order?: number
  /** Human's full name for the From: header (#115). */
  person_name?: string | null
  /** Display-only hint that a PGP key is imported (#57). */
  pgp_key_fingerprint?: string | null
  /** Display-only hint that an S/MIME identity is imported (#338). */
  smime_cert_fingerprint?: string | null
}
/** One entry of `Account.folder_icons`. */
export interface FolderIconRule {
  keyword: string
  icon: string
}
export type AddressbookSummary = any
export type AppSettings = any
export type AttachmentPreviewView = any
export type AttendeeAvailability = any
export type CalendarEvent = any
export type CalendarEventInput = any
export type CalendarSummary = any
export type Contact = any
export type ContactCategoryView = any
export type ContactGroupView = any
export type ContactInput = any
export type ContactPhoto = any
export type CustomTheme = any
export type DatabaseStatusView = any
export type DiscoveredAccount = any
export type DraftReplaceSource = any
export type Email = any
export type EmailEnvelope = any
export type FidoStatusView = any
export type FileEntry = any
export type Folder = any
export type GeocodeResult = any
/**
 * Backend `ImportCalendarReport` (#518) — summary of an `.ics` file
 * import. Typed for real because the import dialog renders every
 * field. Keys are snake_case: the Rust struct has no serde rename.
 */
export interface ImportCalendarReport {
  /** VEVENTs found in the file before dedup / write attempts. */
  total: number
  imported: number
  skipped_duplicates: number
  /** Per-entry failure reasons (recurrence exceptions, write errors). */
  errors: string[]
}
/**
 * Backend `ImportContactsReport` (#484) — summary of a contact file
 * import. Typed for real because the import dialog renders every
 * field. Keys are snake_case: the Rust struct has no serde rename.
 */
export interface ImportContactsReport {
  /** Entries found in the file before dedup / write attempts. */
  total: number
  imported: number
  skipped_duplicates: number
  /** Per-entry failure reasons (unusable rows, write errors). */
  errors: string[]
}
/**
 * Backend `InlineImageView` (#471) — one `cid:`-referenceable image
 * part with its bytes. Typed for real rather than aliased to `any`
 * because the renderer matches on every field.
 */
export interface InlineImagePart {
  partId: number
  /** RFC 2392 Content-ID without angle brackets, when the part had one. */
  contentId: string | null
  filename: string
  mime: string
  base64: string
}
export type InviteSummary = any
export type LinkVerdict = any
export type LoginFlowInit = any
export type MailingListView = any
export type McpServerStatus = any
export type McpToolView = any
export type NextcloudAccount = any
export type NextcloudGroupView = any
export type NextcloudMapsCapability = any
export type NextcloudShareResult = any
export type NextcloudShareRow = any
export type NextcloudUserLookup = any
export type Note = any
export type OfficeOpenResult = any
export type OutboxRowDto = any
export type OutboxSourceRef = any
export type OutgoingEmail = any
export type ParticipantSource = any
export type PdfOpenResult = any
export type PgpKeyStatus = any
export type PgpPublicKeyDto = any
export type ProbedCert = any
/**
 * Backend `unkai_store::profiles::ProfileMeta` (#534). A profile is
 * a fully separate storage universe (own `cache.db`, own SQLCipher
 * key, own settings); this is only what the picker and management
 * UI need before a profile's DB is open.
 */
export interface Profile {
  /** UUID; doubles as directory name + keychain suffix. */
  id: string
  name: string
  icon: ProfileIcon
  /** RFC 3339 timestamps (chrono `DateTime<Utc>`). */
  created_at: string
  last_used_at: string
}
/**
 * A profile's picker icon: a user-chosen emoji, or the name of one
 * of the registered `IconName`s in `Icon.svelte`. Serde tags the
 * Rust enum as `{ kind, value }` with snake_case variant names.
 */
export type ProfileIcon =
  | { kind: 'emoji'; value: string }
  | { kind: 'named'; value: string }
export type ProviderPreset = any
export type RepliedToRef = any
export type SavedDraft = any
export type SearchFilters = any
export type SearchHit = any
export type SearchScope = any
export type SentReceiptStatus = any
export type SettingsSyncStateView = any
export type SmimeCertDto = any
export type SmimeCertStatus = any
/**
 * Backend `unkai_store::profiles::StartupMode` (#534) — which
 * profile(s) the app opens at launch. Serde tags the Rust enum as
 * `{ mode, id? }` with snake_case variant names; only `fixed`
 * carries content: the pinned profile-id list (#552; the backend
 * still parses pre-#552 files where `id` was a single string, but
 * always serves the list shape).
 */
export type StartupMode =
  | { mode: 'fixed'; id: string[] }
  | { mode: 'last_used' }
  | { mode: 'all' }
export type SyncCalendarsReport = any
export type SyncContactsReport = any
export type SyncStatus = any
export type TalkRoom = any
export type Task = any
export type TaskList = any
/**
 * Backend `unkai_core::models::TrustedCert` — one TLS leaf cert the
 * user explicitly trusted for an account. Typed alongside `Account`
 * (#534) since the account row embeds the list.
 */
export interface TrustedCert {
  /** Raw DER bytes as a JSON byte array (Rust `Vec<u8>`). */
  der: number[]
  /** SHA-256 fingerprint, lowercase hex with `:` separators. */
  sha256: string
  host: string
  /** Unix epoch seconds when the cert was trusted. */
  added_at: number
}
export type UrlhausStatus = any
export type WipePolicyView = any
