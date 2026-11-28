//! Deterministic [`FrameCapturer`] implementation backed by recorded frames.
//!
//! Strictly returns `None` for any capture region not fully contained within the current
//! recorded frame bounds (never synthesizing black fallback images) and tracks exact
//! served/unserved counts by region category for transparency in `report.json`.

use gh_app::capture_backend::FrameCapturer;
use gh_core::frame::{FrameBuf, PixelFormat};
use gh_cv::map_state::MapScreenStateMachine;
use gh_platform::wgc_hdr::WgcWindowCaptureOutcome;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServedCaptures {
    pub region_bgr: u64,
    pub region_bgra: u64,
    pub wgc_window: u64,
}

impl ServedCaptures {
    pub fn total(&self) -> u64 {
        self.region_bgr + self.region_bgra + self.wgc_window
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnservedCaptures {
    pub close_or_slider_probe: u64,
    pub full_frame: u64,
    pub minimap_oob: u64,
    pub wgc_window: u64,
    pub other_oob: u64,
    pub empty_frame: u64,
}

impl UnservedCaptures {
    pub fn total(&self) -> u64 {
        self.close_or_slider_probe
            + self.full_frame
            + self.minimap_oob
            + self.wgc_window
            + self.other_oob
            + self.empty_frame
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureCoverageStats {
    pub served: ServedCaptures,
    pub unserved: UnservedCaptures,
}

struct Slot {
    frame: Option<FrameBuf>,
    origin_x: i32,
    origin_y: i32,
    client_w: i32,
    client_h: i32,
    is_hdr: bool,
    stats: CaptureCoverageStats,
}

pub struct ReplayCapturer {
    slot: Mutex<Slot>,
}

impl Default for ReplayCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayCapturer {
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                frame: None,
                origin_x: 0,
                origin_y: 0,
                client_w: 0,
                client_h: 0,
                is_hdr: false,
                stats: CaptureCoverageStats::default(),
            }),
        }
    }

    /// Loads the frame for the current tick. `(origin_x, origin_y)` is the top-left coordinate
    /// of `frame` inside the `(client_w, client_h)` virtual game client (usually `(0, 0)`).
    pub fn set_frame(
        &self,
        frame: Option<FrameBuf>,
        origin_x: i32,
        origin_y: i32,
        client_w: i32,
        client_h: i32,
        is_hdr: bool,
    ) {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        s.frame = frame;
        s.origin_x = origin_x;
        s.origin_y = origin_y;
        s.client_w = client_w;
        s.client_h = client_h;
        s.is_hdr = is_hdr;
    }

    /// Returns the previously loaded frame buffer (if any) so callers can recycle allocations.
    pub fn take_frame(&self) -> Option<FrameBuf> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).frame.take()
    }

    pub fn stats(&self) -> CaptureCoverageStats {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).stats
    }

    fn classify_oob(slot: &Slot, left: i32, top: i32, width: i32, height: i32) -> &'static str {
        if slot.client_w > 0
            && slot.client_h > 0
            && width >= slot.client_w
            && height >= slot.client_h
        {
            return "full_frame";
        }
        if slot.client_w > 0 && slot.client_h > 0 {
            let close_r = MapScreenStateMachine::get_close_probe_rect(
                slot.origin_x,
                slot.origin_y,
                slot.client_w,
                slot.client_h,
            );
            let slider_r = MapScreenStateMachine::get_slider_probe_rect(
                slot.origin_x,
                slot.origin_y,
                slot.client_w,
                slot.client_h,
            );
            if (left == close_r.x && top == close_r.y && width == close_r.width && height == close_r.height)
                || (left == slider_r.x && top == slider_r.y && width == slider_r.width && height == slider_r.height)
            {
                return "close_or_slider_probe";
            }
            if left >= slot.client_w / 2 && top < slot.client_h / 2 {
                return "close_or_slider_probe";
            }
            if left < slot.client_w / 2 && top < slot.client_h / 2 {
                return "minimap_oob";
            }
        }
        "other_oob"
    }

    fn record_oob(slot: &mut Slot, left: i32, top: i32, width: i32, height: i32) {
        match Self::classify_oob(slot, left, top, width, height) {
            "full_frame" => slot.stats.unserved.full_frame += 1,
            "close_or_slider_probe" => slot.stats.unserved.close_or_slider_probe += 1,
            "minimap_oob" => slot.stats.unserved.minimap_oob += 1,
            _ => slot.stats.unserved.other_oob += 1,
        }
    }

    fn slice_region(&self, left: i32, top: i32, width: i32, height: i32, want_bgra: bool) -> Option<FrameBuf> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if width <= 0 || height <= 0 {
            slot.stats.unserved.other_oob += 1;
            return None;
        }
        let Some(ref frame) = slot.frame else {
            slot.stats.unserved.empty_frame += 1;
            return None;
        };

        let rel_x = left.checked_sub(slot.origin_x);
        let rel_y = top.checked_sub(slot.origin_y);
        let (Some(rx), Some(ry)) = (rel_x, rel_y) else {
            Self::record_oob(&mut slot, left, top, width, height);
            return None;
        };
        let fw = frame.width as i32;
        let fh = frame.height as i32;
        let end_x = rx.checked_add(width);
        let end_y = ry.checked_add(height);
        if rx < 0
            || ry < 0
            || end_x.is_none_or(|ex| ex > fw)
            || end_y.is_none_or(|ey| ey > fh)
        {
            Self::record_oob(&mut slot, left, top, width, height);
            return None;
        }

        let dst = crop_frame_format(
            frame,
            rx as usize,
            ry as usize,
            width as usize,
            height as usize,
            if want_bgra {
                PixelFormat::Bgra8
            } else {
                PixelFormat::Bgr8
            },
        )?;
        if want_bgra {
            slot.stats.served.region_bgra += 1;
        } else {
            slot.stats.served.region_bgr += 1;
        }
        Some(dst)
    }
}

fn bpp_for(fmt: PixelFormat) -> Option<usize> {
    match fmt {
        PixelFormat::Bgr8 => Some(3),
        PixelFormat::Bgra8 => Some(4),
        PixelFormat::Gray8 => Some(1),
        _ => None,
    }
}

fn crop_frame_format(
    src: &FrameBuf,
    rx: usize,
    ry: usize,
    w: usize,
    h: usize,
    target_fmt: PixelFormat,
) -> Option<FrameBuf> {
    let dst_bpp = bpp_for(target_fmt)?;
    let mut out = FrameBuf::new_zeroed(w as u32, h as u32, target_fmt);

    match (src.format, target_fmt) {
        (PixelFormat::Bgr8, PixelFormat::Bgr8) | (PixelFormat::Bgra8, PixelFormat::Bgra8) => {
            let bpp = bpp_for(src.format)?;
            let row_bytes = w * bpp;
            for y in 0..h {
                let s_off = (ry + y) * src.stride + rx * bpp;
                let d_off = y * out.stride;
                out.data[d_off..d_off + row_bytes]
                    .copy_from_slice(&src.data[s_off..s_off + row_bytes]);
            }
        }
        (PixelFormat::Bgra8, PixelFormat::Bgr8) => {
            for y in 0..h {
                let s_row = &src.data[(ry + y) * src.stride + rx * 4..];
                let d_row = &mut out.data[y * out.stride..y * out.stride + w * dst_bpp];
                for x in 0..w {
                    let s = x * 4;
                    let d = x * 3;
                    d_row[d] = s_row[s];
                    d_row[d + 1] = s_row[s + 1];
                    d_row[d + 2] = s_row[s + 2];
                }
            }
        }
        (PixelFormat::Bgr8, PixelFormat::Bgra8) => {
            for y in 0..h {
                let s_row = &src.data[(ry + y) * src.stride + rx * 3..];
                let d_row = &mut out.data[y * out.stride..y * out.stride + w * dst_bpp];
                for x in 0..w {
                    let s = x * 3;
                    let d = x * 4;
                    d_row[d] = s_row[s];
                    d_row[d + 1] = s_row[s + 1];
                    d_row[d + 2] = s_row[s + 2];
                    d_row[d + 3] = 255;
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

impl FrameCapturer for ReplayCapturer {
    fn capture_region(&self, left: i32, top: i32, width: i32, height: i32) -> Option<FrameBuf> {
        self.slice_region(left, top, width, height, false)
    }

    fn capture_region_bgra(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        self.slice_region(left, top, width, height, true)
    }

    fn capture_window_client_outcome(
        &self,
        _hwnd: isize,
        _left: i32,
        _top: i32,
        _width: i32,
        _height: i32,
    ) -> WgcWindowCaptureOutcome {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.stats.unserved.wgc_window += 1;
        WgcWindowCaptureOutcome::Failed
    }

    fn window_capture_status(&self) -> String {
        "replay_disabled".to_owned()
    }

    fn release_window_capture(&self) {}

    fn close(&self) {}

    fn is_region_hdr(&self, _left: i32, _top: i32, _width: i32, _height: i32) -> bool {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).is_hdr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_capturer_serves_in_bounds_and_returns_none_with_counters_for_oob() {
        let cap = ReplayCapturer::new();
        let mut hud = FrameBuf::new_zeroed(434, 348, PixelFormat::Bgra8);
        // Stamp distinctive pixel at (10, 20)
        let off = 20 * hud.stride + 10 * 4;
        hud.data[off..off + 4].copy_from_slice(&[11, 22, 33, 240]);
        cap.set_frame(Some(hud), 0, 0, 2560, 1440, false);

        // In-bounds HUD crop succeeds and preserves pixels
        let got_bgra = cap.capture_region_bgra(0, 0, 434, 348).expect("in-bounds bgra");
        assert_eq!(&got_bgra.data[off..off + 4], &[11, 22, 33, 240]);
        let got_bgr = cap.capture_region(10, 20, 4, 4).expect("in-bounds bgr");
        assert_eq!(&got_bgr.data[0..3], &[11, 22, 33]);

        // Right-half probes (close-X / zoom slider) must return None, never black pixels
        assert!(cap.capture_region(2400, 20, 120, 120).is_none());
        assert!(cap.capture_region(2300, 400, 140, 500).is_none());
        // Full-frame request on crop-only recording returns None
        assert!(cap.capture_region(0, 0, 2560, 1440).is_none());
        // Oversized top-left request returns None as minimap_oob
        assert!(cap.capture_region(0, 0, 500, 400).is_none());
        // WGC window capture returns Failed and increments counter
        assert!(matches!(
            cap.capture_window_client_outcome(1, 0, 0, 2560, 1440),
            WgcWindowCaptureOutcome::Failed
        ));

        let stats = cap.stats();
        assert_eq!(stats.served.region_bgra, 1);
        assert_eq!(stats.served.region_bgr, 1);
        assert_eq!(stats.unserved.close_or_slider_probe, 2);
        assert_eq!(stats.unserved.full_frame, 1);
        assert_eq!(stats.unserved.minimap_oob, 1);
        assert_eq!(stats.unserved.wgc_window, 1);
        assert_eq!(stats.unserved.total(), 5);
    }
}
