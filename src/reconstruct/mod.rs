//! Reconstruction: what was actually written, from the keystrokes.
//!
//! * [`buffer`] — tier 0, a generic text buffer replaying editing keys.
//! * [`apps`] and [`editor`] — tier 1, per-app-class rules (single-line
//!   fields, readline shortcuts, shell history, browser field changes) that
//!   decide what each key does to the buffer.

pub mod apps;
pub mod buffer;
pub mod editor;
