<script lang="ts" module>
  /**
   * 49-icon family for the mail client (#179).  Stroke-based 24×24
   * SVGs that inherit `currentColor`, designed by hand to share
   * one visual vocabulary (1.6 stroke, round caps/joins, 2px
   * corner radii).  See `icons/ICON_DESIGN_REFERENCE.md` at the
   * repo root for the full design spec.
   *
   * Use:
   *   <Icon name="reply" />               // default 20px
   *   <Icon name="archive" size={16} />   // bespoke size
   *   <Icon name="loading" class="text-primary-500" />
   *
   * Colour comes from CSS — every icon's stroke is `currentColor`,
   * so wrapping a button in `text-error-500` recolours its icon
   * and label together.
   */
  export type IconName =
    | 'compose' | 'contacts' | 'calendar' | 'meetings'
    | 'global-inbox' | 'files' | 'notes' | 'tasks'
    | 'share-links' | 'settings' | 'read' | 'unread'
    | 'move-to-folder' | 'delete'
    | 'rsvp-accept' | 'rsvp-tentative' | 'rsvp-decline' | 'rsvp-canceled'
    | 'reply' | 'reply-all' | 'forward'
    | 'archive' | 'snooze' | 'flag' | 'star'
    | 'search' | 'filter' | 'sort'
    | 'attachment' | 'download' | 'print'
    | 'spam' | 'block' | 'important'
    | 'drafts' | 'sent' | 'trash'
    | 'refresh' | 'sync' | 'loading'
    | 'add-account' | 'sign-out' | 'lock'
    | 'notification' | 'mute' | 'do-not-disturb'
    | 'encrypted' | 'decrypted' | 'signed' | 'verified'
    // v3 — presence, status, writing tools, navigation
    | 'online' | 'offline' | 'typing' | 'away'
    | 'help' | 'info' | 'warning' | 'error' | 'success'
    | 'translate' | 'spellcheck' | 'dictate'
    | 'save-draft' | 'open-link' | 'open-on-desktop' | 'cloud' | 'today'
    | 'nav-backward' | 'nav-forward' | 'arrow-left' | 'arrow-right'
    // v4 — common UI, tables, editor, history, security
    | 'email-envelope' | 'design-palette' | 'more' | 'more-info'
    | 'full-screen' | 'emoji' | 'open-in-browser'
    | 'table' | 'insert-row-above' | 'insert-row-below' | 'delete-row'
    | 'insert-column-left' | 'insert-column-right' | 'delete-column'
    | 'clear' | 'insert-image' | 'bullet-list' | 'numbered-list'
    | 'align-left' | 'align-center' | 'align-right' | 'justify'
    | 'quote' | 'text-color' | 'highlight-text'
    | 'undo' | 'redo'
    | 'passphrase' | 'security-key'
    // v5 — people categories + map + time + folder ops
    | 'group' | 'address-book' | 'team' | 'location' | 'time' | 'sun'
    | 'add-folder' | 'delete-folder' | 'respond-with-meeting'
    | 'shield-image-blocked'
    // v6 — UI primitives
    | 'close'
</script>

<script lang="ts">
  import Compose from './icons/Compose.svelte'
  import Contacts from './icons/Contacts.svelte'
  import CalendarIcon from './icons/Calendar.svelte'
  import Meetings from './icons/Meetings.svelte'
  import GlobalInbox from './icons/GlobalInbox.svelte'
  import FilesIcon from './icons/Files.svelte'
  import NotesIcon from './icons/Notes.svelte'
  import Tasks from './icons/Tasks.svelte'
  import ShareLinks from './icons/ShareLinks.svelte'
  import SettingsIcon from './icons/Settings.svelte'
  import Read from './icons/Read.svelte'
  import Unread from './icons/Unread.svelte'
  import MoveToFolder from './icons/MoveToFolder.svelte'
  import DeleteIcon from './icons/Delete.svelte'
  import RsvpAccept from './icons/RsvpAccept.svelte'
  import RsvpTentative from './icons/RsvpTentative.svelte'
  import RsvpDecline from './icons/RsvpDecline.svelte'
  import RsvpCanceled from './icons/RsvpCanceled.svelte'
  import Reply from './icons/Reply.svelte'
  import ReplyAll from './icons/ReplyAll.svelte'
  import Forward from './icons/Forward.svelte'
  import Archive from './icons/Archive.svelte'
  import Snooze from './icons/Snooze.svelte'
  import Flag from './icons/Flag.svelte'
  import Star from './icons/Star.svelte'
  import Search from './icons/Search.svelte'
  import Filter from './icons/Filter.svelte'
  import Sort from './icons/Sort.svelte'
  import Attachment from './icons/Attachment.svelte'
  import Download from './icons/Download.svelte'
  import Print from './icons/Print.svelte'
  import Spam from './icons/Spam.svelte'
  import Block from './icons/Block.svelte'
  import Important from './icons/Important.svelte'
  import Drafts from './icons/Drafts.svelte'
  import Sent from './icons/Sent.svelte'
  import Trash from './icons/Trash.svelte'
  import Refresh from './icons/Refresh.svelte'
  import Sync from './icons/Sync.svelte'
  import Loading from './icons/Loading.svelte'
  import AddAccount from './icons/AddAccount.svelte'
  import SignOut from './icons/SignOut.svelte'
  import Lock from './icons/Lock.svelte'
  import Notification from './icons/Notification.svelte'
  import Mute from './icons/Mute.svelte'
  import DoNotDisturb from './icons/DoNotDisturb.svelte'
  import Encrypted from './icons/Encrypted.svelte'
  import Decrypted from './icons/Decrypted.svelte'
  import Signed from './icons/Signed.svelte'
  import Verified from './icons/Verified.svelte'
  // v3
  import Online from './icons/Online.svelte'
  import Offline from './icons/Offline.svelte'
  import Typing from './icons/Typing.svelte'
  import Away from './icons/Away.svelte'
  import Help from './icons/Help.svelte'
  import Info from './icons/Info.svelte'
  import Warning from './icons/Warning.svelte'
  import ErrorIcon from './icons/Error.svelte'
  import Success from './icons/Success.svelte'
  import Translate from './icons/Translate.svelte'
  import Spellcheck from './icons/Spellcheck.svelte'
  import Dictate from './icons/Dictate.svelte'
  import SaveDraft from './icons/SaveDraft.svelte'
  import OpenLink from './icons/OpenLink.svelte'
  import OpenOnDesktop from './icons/OpenOnDesktop.svelte'
  import Cloud from './icons/Cloud.svelte'
  import Today from './icons/Today.svelte'
  import NavBackward from './icons/NavBackward.svelte'
  import NavForward from './icons/NavForward.svelte'
  import ArrowLeft from './icons/ArrowLeft.svelte'
  import ArrowRight from './icons/ArrowRight.svelte'
  // v4
  import EmailEnvelope from './icons/EmailEnvelope.svelte'
  import DesignPalette from './icons/DesignPalette.svelte'
  import More from './icons/More.svelte'
  import MoreInfo from './icons/MoreInfo.svelte'
  import FullScreen from './icons/FullScreen.svelte'
  import Emoji from './icons/Emoji.svelte'
  import OpenInBrowser from './icons/OpenInBrowser.svelte'
  import Table from './icons/Table.svelte'
  import InsertRowAbove from './icons/InsertRowAbove.svelte'
  import InsertRowBelow from './icons/InsertRowBelow.svelte'
  import DeleteRow from './icons/DeleteRow.svelte'
  import InsertColumnLeft from './icons/InsertColumnLeft.svelte'
  import InsertColumnRight from './icons/InsertColumnRight.svelte'
  import DeleteColumn from './icons/DeleteColumn.svelte'
  import Clear from './icons/Clear.svelte'
  import InsertImage from './icons/InsertImage.svelte'
  import BulletList from './icons/BulletList.svelte'
  import NumberedList from './icons/NumberedList.svelte'
  import AlignLeft from './icons/AlignLeft.svelte'
  import AlignCenter from './icons/AlignCenter.svelte'
  import AlignRight from './icons/AlignRight.svelte'
  import JustifyIcon from './icons/Justify.svelte'
  import Quote from './icons/Quote.svelte'
  import TextColor from './icons/TextColor.svelte'
  import HighlightText from './icons/HighlightText.svelte'
  import Undo from './icons/Undo.svelte'
  import Redo from './icons/Redo.svelte'
  import Passphrase from './icons/Passphrase.svelte'
  import SecurityKey from './icons/SecurityKey.svelte'
  // v5
  import Group from './icons/Group.svelte'
  import AddressBook from './icons/AddressBook.svelte'
  import Team from './icons/Team.svelte'
  import Location from './icons/Location.svelte'
  import Time from './icons/Time.svelte'
  import Sun from './icons/Sun.svelte'
  import AddFolder from './icons/AddFolder.svelte'
  import DeleteFolder from './icons/DeleteFolder.svelte'
  import RespondWithMeeting from './icons/RespondWithMeeting.svelte'
  import ShieldImageBlocked from './icons/ShieldImageBlocked.svelte'
  // v6
  import Close from './icons/Close.svelte'
  import type { Component } from 'svelte'

  interface Props {
    name: IconName
    size?: number | string
    class?: string
  }
  let { name, size = 20, class: cls = '' }: Props = $props()

  // Map every IconName to its component.  Static lookup table
  // keeps the dispatch a single property access at render time.
  const map: Record<IconName, Component<{ size?: number | string; class?: string }>> = {
    'compose': Compose,
    'contacts': Contacts,
    'calendar': CalendarIcon,
    'meetings': Meetings,
    'global-inbox': GlobalInbox,
    'files': FilesIcon,
    'notes': NotesIcon,
    'tasks': Tasks,
    'share-links': ShareLinks,
    'settings': SettingsIcon,
    'read': Read,
    'unread': Unread,
    'move-to-folder': MoveToFolder,
    'delete': DeleteIcon,
    'rsvp-accept': RsvpAccept,
    'rsvp-tentative': RsvpTentative,
    'rsvp-decline': RsvpDecline,
    'rsvp-canceled': RsvpCanceled,
    'reply': Reply,
    'reply-all': ReplyAll,
    'forward': Forward,
    'archive': Archive,
    'snooze': Snooze,
    'flag': Flag,
    'star': Star,
    'search': Search,
    'filter': Filter,
    'sort': Sort,
    'attachment': Attachment,
    'download': Download,
    'print': Print,
    'spam': Spam,
    'block': Block,
    'important': Important,
    'drafts': Drafts,
    'sent': Sent,
    'trash': Trash,
    'refresh': Refresh,
    'sync': Sync,
    'loading': Loading,
    'add-account': AddAccount,
    'sign-out': SignOut,
    'lock': Lock,
    'notification': Notification,
    'mute': Mute,
    'do-not-disturb': DoNotDisturb,
    'encrypted': Encrypted,
    'decrypted': Decrypted,
    'signed': Signed,
    'verified': Verified,
    'online': Online,
    'offline': Offline,
    'typing': Typing,
    'away': Away,
    'help': Help,
    'info': Info,
    'warning': Warning,
    'error': ErrorIcon,
    'success': Success,
    'translate': Translate,
    'spellcheck': Spellcheck,
    'dictate': Dictate,
    'save-draft': SaveDraft,
    'open-link': OpenLink,
    'open-on-desktop': OpenOnDesktop,
    'cloud': Cloud,
    'today': Today,
    'nav-backward': NavBackward,
    'nav-forward': NavForward,
    'arrow-left': ArrowLeft,
    'arrow-right': ArrowRight,
    'email-envelope': EmailEnvelope,
    'design-palette': DesignPalette,
    'more': More,
    'more-info': MoreInfo,
    'full-screen': FullScreen,
    'emoji': Emoji,
    'open-in-browser': OpenInBrowser,
    'table': Table,
    'insert-row-above': InsertRowAbove,
    'insert-row-below': InsertRowBelow,
    'delete-row': DeleteRow,
    'insert-column-left': InsertColumnLeft,
    'insert-column-right': InsertColumnRight,
    'delete-column': DeleteColumn,
    'clear': Clear,
    'insert-image': InsertImage,
    'bullet-list': BulletList,
    'numbered-list': NumberedList,
    'align-left': AlignLeft,
    'align-center': AlignCenter,
    'align-right': AlignRight,
    'justify': JustifyIcon,
    'quote': Quote,
    'text-color': TextColor,
    'highlight-text': HighlightText,
    'undo': Undo,
    'redo': Redo,
    'passphrase': Passphrase,
    'security-key': SecurityKey,
    'group': Group,
    'address-book': AddressBook,
    'team': Team,
    'location': Location,
    'time': Time,
    'sun': Sun,
    'add-folder': AddFolder,
    'delete-folder': DeleteFolder,
    'respond-with-meeting': RespondWithMeeting,
    'shield-image-blocked': ShieldImageBlocked,
    'close': Close,
  }
  const Cmp = $derived(map[name])
</script>

{#if Cmp}
  <Cmp {size} class={cls} />
{/if}
