//! TUI (Terminal User Interface) for Mira proxy.
//!
//! This module provides a terminal-based interface for viewing and interacting
//! with captured HTTP traffic.

pub mod app;
mod ui;

pub use app::App;
pub use ui::draw;
