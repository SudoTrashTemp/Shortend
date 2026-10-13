//! ShortEnd GUI: a thin egui shell over `shortend-core`. All copying, checking and reporting
//! happens in the engine on worker threads; this crate only edits a [`form::Form`], starts
//! jobs and displays their [`shortend_core::Event`]s.

pub mod app;
pub mod form;
pub mod live;

pub use app::{ShortEndApp, View};
pub use eframe::egui;
