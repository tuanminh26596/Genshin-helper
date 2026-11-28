//! Narrow capture boundary for `gh-app`.
//!
//! Production binds [`ScreenCapturer`] directly with zero behavioral change; `gh-sim`
//! implements [`FrameCapturer`] to replay recorded frames synchronously through the real
//! `run_tick_worker` and `AppState` code paths.

use gh_core::frame::FrameBuf;
use gh_platform::capture::ScreenCapturer;
use gh_platform::wgc_hdr::{MonitorHdrInfo, WgcWindowCaptureOutcome};

/// Capture interface consumed by `AppState`, `tracking_loop`, and `DiagnosticManager`.
pub trait FrameCapturer: Send + Sync {
    fn capture_region(&self, left: i32, top: i32, width: i32, height: i32) -> Option<FrameBuf>;

    fn capture_region_bgra(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf>;

    fn capture_window_client_outcome(
        &self,
        hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> WgcWindowCaptureOutcome;

    fn capture_window_client(
        &self,
        hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        match self.capture_window_client_outcome(hwnd, left, top, width, height) {
            WgcWindowCaptureOutcome::Frame(frame) => Some(frame),
            WgcWindowCaptureOutcome::WarmingUp | WgcWindowCaptureOutcome::Failed => None,
        }
    }

    fn window_capture_status(&self) -> String;

    fn release_window_capture(&self);

    fn close(&self);

    fn active_hdr_monitor_info(
        &self,
        _left: i32,
        _top: i32,
        _width: i32,
        _height: i32,
    ) -> Option<MonitorHdrInfo> {
        None
    }

    fn is_region_hdr(&self, _left: i32, _top: i32, _width: i32, _height: i32) -> bool {
        false
    }
}

impl FrameCapturer for ScreenCapturer {
    fn capture_region(&self, left: i32, top: i32, width: i32, height: i32) -> Option<FrameBuf> {
        ScreenCapturer::capture_region(self, left, top, width, height)
    }

    fn capture_region_bgra(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        ScreenCapturer::capture_region_bgra(self, left, top, width, height)
    }

    fn capture_window_client_outcome(
        &self,
        hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> WgcWindowCaptureOutcome {
        ScreenCapturer::capture_window_client_outcome(self, hwnd, left, top, width, height)
    }

    fn capture_window_client(
        &self,
        hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        ScreenCapturer::capture_window_client(self, hwnd, left, top, width, height)
    }

    fn window_capture_status(&self) -> String {
        ScreenCapturer::window_capture_status(self)
    }

    fn release_window_capture(&self) {
        ScreenCapturer::release_window_capture(self)
    }

    fn close(&self) {
        ScreenCapturer::close(self)
    }

    fn active_hdr_monitor_info(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<MonitorHdrInfo> {
        ScreenCapturer::active_hdr_monitor_info(self, left, top, width, height)
    }

    fn is_region_hdr(&self, left: i32, top: i32, width: i32, height: i32) -> bool {
        ScreenCapturer::is_region_hdr(self, left, top, width, height)
    }
}
