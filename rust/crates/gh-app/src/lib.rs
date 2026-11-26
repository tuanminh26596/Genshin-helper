//! Core application runtime library for `GenshinHelper`, shared between the production
//! `GenshinHelper.exe` binary and the dev-only `gh-sim` deterministic replay harness.

pub mod callbacks;
pub mod capture_backend;
pub mod config_watcher;
pub mod diagnostics;
pub mod log_buffer;
pub mod minimap_recorder;
pub mod mmap_guide;
pub mod mmap_view;
pub mod session_recorder;
pub mod state;
pub mod tracking_loop;
pub mod tracking_stack;
pub mod updater;
