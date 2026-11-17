//! `brain`: a shadow assistant that captures what you type and makes it searchable.
//!
//! Pipeline: [`capture`] (keystrokes + focus → append-only [`eventlog`]) →
//! [`ingest`] (log → [`sessions`] with [`reconstruct`]ed text → SQLite
//! [`store`] with full-text index) → [`query`] (`search` over sessions, `text`
//! over the log).

pub mod capture;
pub mod datadir;
pub mod event;
pub mod eventlog;
pub mod ingest;
pub mod query;
pub mod reconstruct;
pub mod sessions;
pub mod store;
pub mod timefmt;
