//! The transport-agnostic application layer of Unkai Mail (#476).
//!
//! Everything here used to be a `#[tauri::command]` body (or a helper
//! for one) inside a single 16 500-line `src-tauri/src/main.rs`.  It
//! moved out so that:
//!
//! * the application logic can be tested without a Tauri runtime;
//! * the module boundaries are reviewable — one module per domain,
//!   matching the frontend's `ui/src/lib/api/` split from #473 exactly,
//!   so a command and its typed wrapper are always named the same and
//!   always live in the same-named file on both sides;
//! * a non-desktop deployment becomes possible at all.
//!
//! **This crate must never depend on `tauri`.**  The one thing it
//! genuinely needs from the shell — "tell the user something happened"
//! — goes through the [`UiNotifier`](notify::UiNotifier) trait, which
//! `src-tauri` implements on `AppHandle`.  `src-tauri/src/main.rs` is
//! now thin `#[tauri::command]` shims plus the desktop shell (tray,
//! menus, windows, deep links, launch argv).
//!
//! ## Layout
//!
//! | module | mirrors |
//! |---|---|
//! | [`accounts`] | `api/accounts.ts` |
//! | [`calendar`] | `api/calendar.ts` |
//! | [`compose`] | `api/compose.ts` |
//! | [`contacts`] | `api/contacts.ts` |
//! | [`crypto`] | `api/crypto.ts` |
//! | [`mail`] | `api/mail.ts` |
//! | [`nextcloud`] | `api/nextcloud.ts` |
//! | [`notes`] | `api/notes.ts` |
//! | [`profiles`] | `api/profiles.ts` |
//! | [`settings`] | `api/settings.ts` |
//! | [`system`] | `api/system.ts` |
//! | [`talk`] | `api/talk.ts` |
//! | [`tasks`] | `api/tasks.ts` |
//!
//! Plus the support modules: [`notify`] (the `UiNotifier` seam),
//! [`state`] (managed state types + [`AppContext`](state::AppContext)),
//! [`support`] (helpers more than one domain needs),
//! [`crypto_bridge`] (the `CryptoBridge` impl the protocol crates use),
//! [`background`] (the startup loops), and [`geocode`].

pub mod background;
pub mod crypto_bridge;
pub mod geocode;
pub mod notify;
pub mod state;
pub mod support;

pub mod accounts;
pub mod calendar;
pub mod compose;
pub mod contacts;
/// CSV parsing for `contacts::import_contacts_file` (#484) — private
/// because only the contacts domain reads CSV.
mod contacts_csv;
pub mod crypto;
pub mod mail;
pub mod nextcloud;
pub mod notes;
pub mod profiles;
pub mod settings;
pub mod system;
pub mod talk;
pub mod tasks;

pub use notify::UiNotifier;
pub use state::AppContext;
