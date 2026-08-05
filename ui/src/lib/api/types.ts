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

export type Account = any
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
export type SyncCalendarsReport = any
export type SyncContactsReport = any
export type SyncStatus = any
export type TalkRoom = any
export type Task = any
export type TaskList = any
export type TrustedCert = any
export type UrlhausStatus = any
export type WipePolicyView = any
