//! OCR stub for Linux.
//!
//! The Windows version uses WinRT `OcrEngine`. On Linux we provide a stub that
//! always returns an empty string. Tesseract integration can be added later.

use gh_core::frame::FrameView;
use gh_core::traits::OcrProvider;
use gh_core::CoreResult;

/// No-op OCR provider for Linux (WinRT OcrEngine not available).
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxOcrProvider;

/// Alias for cross-platform `gh_platform::win_ocr::WindowsOcrService` parity.
pub type WindowsOcrService = LinuxOcrProvider;

impl LinuxOcrProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn try_new() -> CoreResult<Self> {
        Ok(Self)
    }

    pub fn recognizer_language_tag(&self) -> &str {
        "none"
    }
}

impl OcrProvider for LinuxOcrProvider {
    fn recognize_text(&self, _frame: FrameView<'_>) -> CoreResult<String> {
        // Return empty string — OCR is used for underground floor detection
        // which gracefully falls back to position-based detection when empty.
        Ok(String::new())
    }
}
