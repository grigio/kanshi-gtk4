//! kanshi-gtk4 — arrange displays and persist the layout as a kanshi profile.
//!
//! The library target exists so the apply/rollback engine can be exercised by
//! the integration tests in `tests/` without opening a window.

pub mod application;
pub mod apply;
pub mod backend;
pub mod config;
pub mod kanshi;
pub mod model;
pub mod session;
pub mod ui;
pub mod util;
pub mod watchdog;
