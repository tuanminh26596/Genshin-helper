//! Linux platform integration crate: X11 screen capture, evdev global hotkeys,
//! D-Bus system tray, PCM audio, POSIX locale, and Unix-socket single-instance mutex.
//!
//! Acyclic dependency: depends ONLY on `gh-core`.
//!
//! This crate mirrors the public API surface of `gh-win32` so that `gh-hud`,
//! `gh-app`, and `gh-cv` compile on Linux with zero changes to their logic.

pub mod audio;
pub mod capture;
pub mod game_settings;
pub mod hotkey;
pub mod hotkey_conflicts;
pub mod locale;
pub mod runtime;
pub mod switcher;
pub mod sys_info;
pub mod tray;
pub mod wayland_capture;
pub mod wgc_hdr;
pub mod win_ocr;

pub use gh_core::traits::{ConflictAdvisor, LocaleProvider, OcrProvider};
