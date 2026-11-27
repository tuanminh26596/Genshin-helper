//! Core application runtime library for `GenshinHelper`, shared between the production
//! `GenshinHelper.exe` binary and the dev-only `gh-sim` deterministic replay harness.

/// Crate version exported at compile time by `gh-app/build.rs`.
pub const BUILD_APP_VERSION: &str = env!("GH_APP_VERSION");
/// Full git HEAD commit SHA exported at compile time by `gh-app/build.rs` (or `"unknown"`).
pub const BUILD_GIT_COMMIT: &str = env!("GH_GIT_COMMIT");

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
