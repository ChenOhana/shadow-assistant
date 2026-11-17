//! Reconstruction: what was actually written, from the keystrokes.
//!
//! * [`buffer`] — tier 0, a generic text buffer replaying editing keys.
//! * [`apps`] and [`editor`] — tier 1, per-app-class rules (single-line
//!   fields, readline shortcuts, shell history, browser field changes) that
//!   decide what each key does to the buffer.
//!
//! Tiers 0 and 1 are used by the session engine ([`crate::sessions`]) and,
//! without sessions, by `brain query text` ([`crate::query::reconstruct_windows`]).
//! * [`vscode`] and [`tier2`] — tier 2, VS Code sessions replayed into the
//!   real editor, sandboxed (opt-in: `brain run --vscode-replay`).

pub mod apps;
pub mod buffer;
pub mod editor;
pub mod tier2;
pub mod vscode;
