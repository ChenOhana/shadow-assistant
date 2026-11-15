//! `brain`: a shadow assistant that captures what you type and makes it searchable.
//!
//! Pipeline: [`capture`] (keystrokes + focus → append-only [`eventlog`]) →
//! [`ingest`] (log → SQLite [`store`]) → query.

pub mod datadir;
pub mod event;
pub mod eventlog;
pub mod ingest;
pub mod store;
pub mod timefmt;
