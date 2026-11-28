//! Linux platform integration crate: X11 screen capture, evdev global hotkeys,
//! D-Bus system tray, PCM audio, POSIX locale, and Unix-socket single-instance mutex.
//!
//! Acyclic dependency: depends ONLY on `gh-core`.
//!
//! This crate mirrors the public API surface of `gh-win32` so that `gh-hud`,
//! `gh-app`, and `gh-cv` compile on Linux with zero changes to their logic.

#[cfg(target_os = "linux")]
pub mod audio;
#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod game_settings;
#[cfg(target_os = "linux")]
pub mod hotkey;
#[cfg(target_os = "linux")]
pub mod hotkey_conflicts;
#[cfg(target_os = "linux")]
pub mod locale;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod switcher;
#[cfg(target_os = "linux")]
pub mod sys_info;
#[cfg(target_os = "linux")]
pub mod tray;
#[cfg(target_os = "linux")]
pub mod wayland_capture;
#[cfg(target_os = "linux")]
pub mod wgc_hdr;
#[cfg(target_os = "linux")]
pub mod win_ocr;

pub use gh_core::traits::{ConflictAdvisor, LocaleProvider, OcrProvider};
