//! Windows platform integration crate: flicker-free GDI capture (`SRCCOPY` only),
//! read-only low-level keyboard/mouse hooks (strictly zero `SendInput`), hotkey conflict
//! analysis, UIPI/borderless settings, in-process WinRT OCR, tray, PCM audio, and switcher.
//!
//! Acyclic dependency: depends ONLY on `gh-core`.

pub mod audio;
pub mod authenticode;
pub mod capture;
pub mod game_settings;
pub mod hdr_tonemap;
pub mod hotkey;
pub mod hotkey_conflicts;
pub mod locale;
pub mod runtime;
pub mod switcher;
pub mod sys_info;
pub mod tray;
pub mod wgc_hdr;
pub mod win_ocr;

pub use gh_core::traits::{ConflictAdvisor, LocaleProvider, OcrProvider};
