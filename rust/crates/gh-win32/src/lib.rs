//! Windows platform integration crate: flicker-free GDI capture (`SRCCOPY` only),
//! read-only low-level keyboard/mouse hooks (strictly zero `SendInput`), hotkey conflict
//! analysis, UIPI/borderless settings, in-process WinRT OCR, tray, PCM audio, and switcher.
//!
//! Acyclic dependency: depends ONLY on `gh-core`.

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod game_settings;
#[cfg(windows)]
pub mod hdr_tonemap;
#[cfg(windows)]
pub mod hotkey;
#[cfg(windows)]
pub mod hotkey_conflicts;
#[cfg(windows)]
pub mod locale;
#[cfg(windows)]
pub mod runtime;
#[cfg(windows)]
pub mod switcher;
#[cfg(windows)]
pub mod sys_info;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod wgc_hdr;
#[cfg(windows)]
pub mod win_ocr;

pub use gh_core::traits::{ConflictAdvisor, LocaleProvider, OcrProvider};
