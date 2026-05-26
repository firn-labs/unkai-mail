//! Unkai Nextcloud — integration with Nextcloud APIs.
//!
//! # What this crate does
//!
//! - **Login Flow v2** (`auth`) — the modern, browser-based auth flow
//!   used by the official Nextcloud desktop client. No passwords ever
//!   touch the app: the user authorises in their browser and the server
//!   hands us back a revocable *app password*.
//! - **Capability detection** (`capabilities`) — asks the server which
//!   apps (Talk, Files, CalDAV, CardDAV) are installed so the UI can
//!   show or hide features accordingly.
//! - **Files** (`files`) — WebDAV browse / download for "attach from
//!   Nextcloud". Public shares (`shares`) for "send as link".
//! - **Talk** (`talk`) — list, create, and add participants to Talk
//!   rooms. The "create Talk room from email thread" flow lives here.

pub mod auth;
pub mod capabilities;
pub mod client;
pub mod files;
pub mod notes;
pub mod shares;
pub mod talk;
pub mod user;

pub use auth::{LoginFlowInit, LoginFlowResult, poll_login, start_login};
pub use capabilities::fetch_capabilities;
pub use files::{
    FileEntry, create_directory, delete_path, download_file, fetch_preview, list_directory,
    propfind_fileid, upload_file,
};
pub use notes::{
    NewNote, Note, NoteUpdate, create_note, delete_note, get_note, list_notes, update_note,
};
pub use shares::{
    PublicShare, PublicShareInfo, create_public_share, delete_share, list_public_shares,
    update_public_share, update_share_label,
};
pub use talk::{
    CreateRoomOptions, ParticipantSource, RoomType, TalkRoom, add_participant, create_room,
    delete_room, list_rooms, rename_room, set_room_public,
};
pub use user::{
    NextcloudCircle, NextcloudUserMatch, NextcloudUserProfile, NextcloudUserSummary,
    fetch_circle_member_ids, fetch_current_user, fetch_group_member_ids, fetch_my_circles,
    fetch_my_groups, fetch_user_profile, find_user_by_email,
};
