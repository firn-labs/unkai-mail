//! Nimbus CardDAV — read-only contact sync against CardDAV servers
//! (currently Nextcloud).
//!
//! # Why a hand-rolled DAV client
//!
//! Rust's CardDAV / CalDAV crate landscape is sparse — the maintained
//! options are heavyweight server libraries, not lean clients. The
//! requests we actually need are small:
//!
//! - **PROPFIND** (Depth: 1) on the user's addressbook home to list
//!   their addressbooks and pick up `displayname` + `getctag` +
//!   `sync-token` for each.
//! - **REPORT** with `sync-collection` against each addressbook to
//!   enumerate added / changed / deleted vCards since the last token.
//! - **REPORT** with `addressbook-multiget` (only when needed) to fetch
//!   the vCard bodies for changed hrefs.
//!
//! Each of those fits in under twenty lines of XML. So we hand-roll
//! the requests with `reqwest` + `quick-xml` and parse responses with
//! event-driven `quick-xml` reads. Easier to debug than wrestling a
//! generic DAV abstraction, and we never have to wonder which obscure
//! propstat shape the lib happens to expect.
//!
//! # Sync model
//!
//! We use **WebDAV sync-collection** (RFC 6578) — the server hands us
//! a `sync-token` whose semantics are opaque to us; we send it back on
//! the next sync to ask "what changed since". This is true incremental
//! sync, not "fetch the world and diff client-side".
//!
//! On the very first sync the token is empty, which the server reads as
//! "give me everything"; the response also carries the next token so
//! every subsequent run is incremental.
//!
//! Nextcloud also exposes the older `getctag` extension. We treat it as
//! an optional cheap pre-check: if the ctag hasn't changed since last
//! sync we skip the REPORT entirely. Belt-and-braces — the sync token
//! alone would be enough.
//!
//! # What this crate does NOT do
//!
//! - **Storage** — `RawContact` and `SyncDelta` are pure data; the caller
//!   (the Tauri command in `src-tauri`) decides where to put them. This
//!   keeps the crate testable without dragging the SQLite layer in.
//! - **Discovery via `.well-known/carddav`** — Nextcloud's path is
//!   stable (`/remote.php/dav/addressbooks/users/<user>/`) so we go
//!   straight to it. A general DAV client would do the well-known
//!   redirect dance.
//!
//! # Two-way sync
//!
//! `write.rs` implements PUT (create/update) and DELETE with
//! `If-Match` / `If-None-Match: *` preconditions for optimistic
//! concurrency — the server, not us, decides when a conflict
//! happened. Generated vCards round-trip through the parser, so a
//! contact created here is byte-equivalent (after re-parse) to one
//! synced from elsewhere.

pub mod client;
pub mod discovery;
pub mod sync;
pub mod vcard;
pub mod write;
mod xml_util;

pub use discovery::{Addressbook, list_addressbooks};
pub use sync::{RawContact, SyncDelta, sync_addressbook};
pub use vcard::{
    ParsedVcard, VcardAddress, VcardEmail, VcardImpp, VcardPhone, VcardStructuredName, build_vcard,
    parse_vcard,
};
pub use write::{WriteOutcome, create_contact, delete_contact, update_contact};
