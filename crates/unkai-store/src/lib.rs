//! Unkai Store — local data persistence and caching.
//!
//! Handles offline storage, caching of emails/contacts/events,
//! and account credential management via the OS keychain.

pub mod account_store;
pub mod app_settings;
pub mod cache;
pub mod credentials;
pub mod fido;
pub mod link_check;
pub mod nextcloud_store;
pub mod settings_bundle;
pub mod settings_sync;

pub use cache::{Cache, DueMessageReminder, OutboxEnqueue, OutboxRow};
