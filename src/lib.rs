//! `sweep` — find and reclaim stale developer caches.
//!
//! The crate is split so the scanner can be used without a terminal:
//!
//! * [`rules`] decides what counts as a build artifact,
//! * [`scan`] walks the filesystem in parallel and measures what it finds,
//! * [`remove`] enforces the delete-safety fence and writes the audit log,
//! * [`cli`] and [`tui`] are the two front ends,
//! * [`theme`] and [`util`] handle presentation.

pub mod cli;
pub mod remove;
pub mod rules;
pub mod scan;
pub mod theme;
pub mod tui;
pub mod util;
