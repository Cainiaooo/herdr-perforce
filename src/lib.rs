//! Core library for the Herdr Perforce pane.
//!
//! The domain layer deliberately has no dependency on a terminal UI, process
//! implementation, or Herdr transport. This keeps P4 parsing, freshness and
//! state-transition contracts testable without touching a real workspace.

pub mod app;
pub mod domain;
pub mod p4;
pub mod panel_restore;
pub mod submit_provider;
pub mod tui;
