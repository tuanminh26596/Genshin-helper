//! Pure Unity CanvasScaler (`1920x1080 Expand`) UI layout geometry, HDR clipping detector,
//! and borderless windowed launch args (split from `src/game_settings.py`).

use crate::frame::{FrameView, PixelFormat, RectI32};
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};

pub const REF_CANVAS_W: u32 = 1920;
pub const REF_CANVAS_H: u32 = 1080;
pub const REF_ASPECT_16_9: f64 = 1920.0 / 1080.0;

/// Ultrawide (>16:9) horizontal HUD safe-area inset ratio per extra reference-canvas pixel
/// beyond 16:9 (`REF_CANVAS_W = 1920`).
///
/// Calibrated from the single measured 21:9 capture (`2560x1080`, `01_game.png`):
/// - Extra canvas width beyond 16:9: `2560 - 1920 = 640` canvas px (`scale_ui = 1.0`).
/// - Measured top-left HUD shift (minimap + Paimon + quest icons): `+72 px X, +0 px Y`.
/// - Ratio: `72.0 / 640.0 = 0.1125` (`9 / 80`).
///
/// Only `2560x1080` is backed by a real capture; all other ultrawide widths
/// (e.g. `3440x1440`, `3840x1600`, `5120x1440` 32:9) are linear extrapolations of this ratio
/// until additional ultrawide captures are collected.
pub const ULTRAWIDE_INSET_X_RATIO: f64 = 72.0 / 640.0;

pub const MINIMAP_RECT_DESKTOP_1080P: RectI32 = RectI32 {
    x: 59,
    y: 15,
    width: 218,
    height: 218,
};
pub const MINIMAP_RECT_GAMEPAD_1080P: RectI32 = RectI32 {
    x: 117,
    y: 45,
    width: 182,
    height: 182,
};
pub const PAIMON_RECT_DESKTOP_1080P: RectI32 = RectI32 {
    x: 25,
    y: 12,
    width: 68,
    height: 77,
};
pub const PAIMON_RECT_GAMEPAD_1080P: RectI32 = RectI32 {
    x: 89,
    y: 42,
    width: 57,
    height: 64,
};

pub const HDR_WHITE_LUMA_THRESHOLD: usize = 250;
pub const HDR_CLIP_P95_THRESHOLD: f64 = 250.0;
pub const HDR_CLIP_WHITE_RATIO_THRESHOLD: f64 = 0.35;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AspectLabel {
    Ratio16x9,
    Ratio32x9,
    Ratio21x9,
    Ratio16x10,
    Ratio4x3,
    Ultrawide,
    Tall,
}

impl AspectLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ratio16x9 => "16:9",
            Self::Ratio32x9 => "32:9",
            Self::Ratio21x9 => "21:9",
            Self::Ratio16x10 => "16:10",
            Self::Ratio4x3 => "4:3",
            Self::Ultrawide => "ultrawide",
            Self::Tall => "tall",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UiLayout {
    pub viewport_w: u32,
    pub viewport_h: u32,
    pub scale_ui: f64,
    pub minimap_rect: RectI32,
    pub is_gamepad: bool,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub aspect_ratio: f64,
    pub aspect_label: AspectLabel,
    pub minimap_rect_canvas: RectI32,
    pub paimon_rect_canvas: RectI32,
    pub paimon_rect: RectI32,
    pub minimap_maybe_rect: RectI32,
    pub paimon_maybe_rect: RectI32,
    pub uid_rect_canvas: RectI32,
    pub uid_rect: RectI32,
    pub uid_maybe_rect: RectI32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct HdrClippingReport {
    pub clipped: bool,
    pub p95: f64,
    pub white_ratio: f64,
}

fn classify_aspect(aspect: f64) -> AspectLabel {
    if (aspect - REF_ASPECT_16_9).abs() < 0.03 {
        AspectLabel::Ratio16x9
    } else if aspect >= 2.80 {
        AspectLabel::Ratio32x9
    } else if (2.20..2.80).contains(&aspect) {
        AspectLabel::Ratio21x9
    } else if (1.55..=1.65).contains(&aspect) {
        AspectLabel::Ratio16x10
    } else if (1.25..=1.40).contains(&aspect) {
        AspectLabel::Ratio4x3
    } else if aspect > REF_ASPECT_16_9 {
        AspectLabel::Ultrawide
    } else {
        AspectLabel::Tall
    }
}

fn scale_and_clamp_rect(rect_canvas: RectI32, scale: f64, frame_w: i32, frame_h: i32) -> RectI32 {
    let x = ((rect_canvas.x as f64) * scale).round_ties_even() as i32;
    let y = ((rect_canvas.y as f64) * scale).round_ties_even() as i32;
    let w = ((rect_canvas.width as f64) * scale).round_ties_even() as i32;
    let h = ((rect_canvas.height as f64) * scale).round_ties_even() as i32;

    let x = x.clamp(0, frame_w - 1);
    let y = y.clamp(0, frame_h - 1);
    let width = w.clamp(1, frame_w - x);
    let height = h.clamp(1, frame_h - y);
    RectI32 {
        x,
        y,
        width,
        height,
    }
}

fn ultrawide_hud_inset_x_canvas(aspect: f64, canvas_w_exact: f64) -> i32 {
    if aspect <= REF_ASPECT_16_9 + 1e-6 {
        return 0;
    }
    let extra_canvas_w = (canvas_w_exact - REF_CANVAS_W as f64).max(0.0);
    (extra_canvas_w * ULTRAWIDE_INSET_X_RATIO).round_ties_even() as i32
}

/// Computes Unity CanvasScaler (`ScaleWithScreenSize`, `1920x1080` Expand) UI geometry
/// across 16:9, 21:9, 16:10, 4:3, 32:9, and Gamepad safe-area modes.
pub fn compute_ui_layout(w: u32, h: u32, is_gamepad: bool) -> CoreResult<UiLayout> {
    if w == 0 || h == 0 {
        return Err(CoreError::InvalidArgument(format!(
            "invalid frame dimensions: ({w}, {h})"
        )));
    }

    let fw = w as i32;
    let fh = h as i32;
    let aspect = (w as f64) / (h as f64);

    let (scale_ui, canvas_w, canvas_h, inset_x_canvas) = if (aspect - REF_ASPECT_16_9).abs() <= 1e-6
    {
        (
            h as f64 / REF_CANVAS_H as f64,
            REF_CANVAS_W,
            REF_CANVAS_H,
            0,
        )
    } else if aspect > REF_ASPECT_16_9 {
        let s = h as f64 / REF_CANVAS_H as f64;
        let cw_exact = (w as f64) / s;
        let inset = ultrawide_hud_inset_x_canvas(aspect, cw_exact);
        (s, cw_exact as u32, REF_CANVAS_H, inset)
    } else {
        let s = w as f64 / REF_CANVAS_W as f64;
        (s, REF_CANVAS_W, ((h as f64) / s) as u32, 0)
    };

    let base_minimap = if is_gamepad {
        MINIMAP_RECT_GAMEPAD_1080P
    } else {
        MINIMAP_RECT_DESKTOP_1080P
    };
    let minimap_rect_canvas = RectI32 {
        x: base_minimap.x + inset_x_canvas,
        ..base_minimap
    };
    let base_paimon = if is_gamepad {
        PAIMON_RECT_GAMEPAD_1080P
    } else {
        PAIMON_RECT_DESKTOP_1080P
    };
    let paimon_rect_canvas = RectI32 {
        x: base_paimon.x + inset_x_canvas,
        ..base_paimon
    };

    let maybe_ref_w = if inset_x_canvas > 0 {
        REF_CANVAS_W as f64
    } else {
        canvas_w as f64
    };
    let paimon_maybe_canvas = RectI32 {
        x: inset_x_canvas,
        y: 0,
        width: (maybe_ref_w * 0.10) as i32,
        height: ((canvas_h as f64) * 0.10) as i32,
    };
    let minimap_maybe_canvas = RectI32 {
        x: inset_x_canvas,
        y: 0,
        width: (maybe_ref_w * 0.18) as i32,
        height: ((canvas_h as f64) * 0.22) as i32,
    };

    let uid_maybe_left = ((canvas_w as f64) * 0.88) as i32;
    let uid_maybe_top = ((canvas_h as f64) * 0.97) as i32;
    let uid_maybe_canvas = RectI32 {
        x: uid_maybe_left,
        y: uid_maybe_top,
        width: (canvas_w as i32 - uid_maybe_left).max(1),
        height: (canvas_h as i32 - uid_maybe_top).max(1),
    };

    let cw_i = canvas_w as i32;
    let ch_i = canvas_h as i32;
    let uid_x = ((canvas_w as f64) - (canvas_w as f64) * (1.0 - 0.865)).ceil() as i32;
    let uid_y = ((canvas_h as f64) - (REF_CANVAS_H as f64) * (1.0 - 0.9755)).ceil() as i32;
    let uid_w = ((REF_CANVAS_W as f64) * 0.11).ceil() as i32;
    let uid_h = ((REF_CANVAS_W as f64) * 0.0938 * 0.11).ceil() as i32;
    let uid_x = uid_x.clamp(0, cw_i - 1);
    let uid_y = uid_y.clamp(0, ch_i - 1);
    let uid_rect_canvas = RectI32 {
        x: uid_x,
        y: uid_y,
        width: uid_w.clamp(1, cw_i - uid_x),
        height: uid_h.clamp(1, ch_i - uid_y),
    };

    Ok(UiLayout {
        viewport_w: w,
        viewport_h: h,
        scale_ui,
        minimap_rect: scale_and_clamp_rect(minimap_rect_canvas, scale_ui, fw, fh),
        is_gamepad,
        canvas_w,
        canvas_h,
        aspect_ratio: aspect,
        aspect_label: classify_aspect(aspect),
        minimap_rect_canvas,
        paimon_rect_canvas,
        paimon_rect: scale_and_clamp_rect(paimon_rect_canvas, scale_ui, fw, fh),
        minimap_maybe_rect: scale_and_clamp_rect(minimap_maybe_canvas, scale_ui, fw, fh),
        paimon_maybe_rect: scale_and_clamp_rect(paimon_maybe_canvas, scale_ui, fw, fh),
        uid_rect_canvas,
        uid_rect: scale_and_clamp_rect(uid_rect_canvas, scale_ui, fw, fh),
        uid_maybe_rect: scale_and_clamp_rect(uid_maybe_canvas, scale_ui, fw, fh),
    })
}

/// Computes the top-left tracker HUD capture crop `(hud_w, hud_h)` in client pixels
/// covering `layout.minimap_rect` (plus calibration padding) and `layout.paimon_maybe_rect`.
///
/// Shared by `gh-app::tracking_loop` and `gh-sim::frames` so live capture and offline
/// frame regression checks stay in lockstep.
pub fn compute_tracker_hud_crop(layout: &UiLayout) -> (i32, i32) {
    let cw = layout.viewport_w as i32;
    let ch = layout.viewport_h as i32;
    let mm = layout.minimap_rect;
    let pm = layout.paimon_maybe_rect;
    let cal_pad_x = (48.0 * layout.scale_ui).round() as i32;
    let cal_pad_y = (28.0 * layout.scale_ui).round() as i32;
    let hud_w = cw.min((mm.x + mm.width + cal_pad_x).max(pm.x + pm.width));
    let hud_h = ch.min((mm.y + mm.height + cal_pad_y).max(pm.y + pm.height));
    (hud_w, hud_h)
}

/// Detects washed-out SDR clamping typical of Windows Auto-HDR captured via GDI BitBlt.
pub fn detect_hdr_clipping(frame: FrameView<'_>) -> HdrClippingReport {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let ch = frame.format.channels();
    if w == 0 || h == 0 || frame.stride < w * ch {
        return HdrClippingReport::default();
    }
    let required_bytes = (h - 1)
        .saturating_mul(frame.stride)
        .saturating_add(w.saturating_mul(ch));
    if frame.data.len() < required_bytes {
        return HdrClippingReport::default();
    }

    let mut hist = [0u64; 256];
    for y in 0..h {
        let row = &frame.data[y * frame.stride..y * frame.stride + w * ch];
        match frame.format {
            PixelFormat::Gray8 => {
                for &luma in row {
                    hist[luma as usize] += 1;
                }
            }
            PixelFormat::Bgr8 | PixelFormat::Bgra8 => {
                for px in row.chunks_exact(ch) {
                    let b = px[0] as u32;
                    let g = px[1] as u32;
                    let r = px[2] as u32;
                    let luma = ((3735 * b + 19235 * g + 9798 * r + 16_384) >> 15).min(255) as usize;
                    hist[luma] += 1;
                }
            }
            PixelFormat::Rgba8 => {
                for px in row.as_chunks::<4>().0 {
                    let r = px[0] as u32;
                    let g = px[1] as u32;
                    let b = px[2] as u32;
                    let luma = ((9798 * r + 19235 * g + 3735 * b + 16_384) >> 15).min(255) as usize;
                    hist[luma] += 1;
                }
            }
        }
    }

    let total = (w * h) as u64;
    if total == 0 {
        return HdrClippingReport::default();
    }

    let target = 0.95 * (total as f64);
    let mut cdf = 0u64;
    let mut p95_idx = 255usize;
    for (idx, &count) in hist.iter().enumerate() {
        cdf += count;
        if (cdf as f64) >= target {
            p95_idx = idx;
            break;
        }
    }
    let p95 = p95_idx.min(255) as f64;
    let white_count: u64 = hist[HDR_WHITE_LUMA_THRESHOLD..].iter().sum();
    let white_ratio = (white_count as f64) / (total as f64);
    let clipped = p95 >= HDR_CLIP_P95_THRESHOLD && white_ratio >= HDR_CLIP_WHITE_RATIO_THRESHOLD;

    HdrClippingReport {
        clipped,
        p95,
        white_ratio,
    }
}

/// Builds Unity borderless windowed command-line arguments without altering game files.
pub fn build_borderless_launch_args(
    width: u32,
    height: u32,
    exe_path: Option<&str>,
) -> CoreResult<Vec<String>> {
    if width == 0 || height == 0 {
        return Err(CoreError::InvalidArgument(format!(
            "invalid screen dimensions: ({width}, {height})"
        )));
    }
    let mut args = Vec::with_capacity(7);
    if let Some(exe) = exe_path
        && !exe.is_empty()
    {
        args.push(exe.to_owned());
    }
    args.extend([
        "-popupwindow".to_owned(),
        "-screen-fullscreen".to_owned(),
        "0".to_owned(),
        "-screen-width".to_owned(),
        width.to_string(),
        "-screen-height".to_owned(),
        height.to_string(),
    ]);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FrameBuf;

    #[test]
    fn compute_ui_layout_matches_python_across_all_7_resolutions() {
        let l1080 = compute_ui_layout(1920, 1080, false).expect("1080p");
        assert_eq!(l1080.aspect_label, AspectLabel::Ratio16x9);
        assert_eq!(l1080.aspect_label.as_str(), "16:9");
        assert!((l1080.scale_ui - 1.0).abs() < 1e-9);
        assert_eq!(l1080.minimap_rect, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(l1080.paimon_rect, PAIMON_RECT_DESKTOP_1080P);

        let gp = compute_ui_layout(1920, 1080, true).expect("1080p gamepad");
        assert!(gp.is_gamepad);
        assert_eq!(gp.minimap_rect, MINIMAP_RECT_GAMEPAD_1080P);
        assert_eq!(gp.paimon_rect, PAIMON_RECT_GAMEPAD_1080P);

        let l1440 = compute_ui_layout(2560, 1440, false).expect("1440p");
        assert_eq!(l1440.aspect_label, AspectLabel::Ratio16x9);
        assert!((l1440.scale_ui - (2560.0 / 1920.0)).abs() < 1e-6);
        assert_eq!(
            l1440.minimap_rect,
            RectI32 {
                x: 79,
                y: 20,
                width: 291,
                height: 291
            }
        );

        let uw21 = compute_ui_layout(3440, 1440, false).expect("21:9");
        assert_eq!(uw21.aspect_label, AspectLabel::Ratio21x9);
        assert_eq!(uw21.canvas_h, 1080);
        assert_eq!(uw21.canvas_w, 2580);
        assert_eq!(uw21.minimap_rect.width, 291);

        let r16_10 = compute_ui_layout(1920, 1200, false).expect("16:10");
        assert_eq!(r16_10.aspect_label, AspectLabel::Ratio16x10);
        assert_eq!(r16_10.canvas_w, 1920);
        assert_eq!(r16_10.canvas_h, 1200);
        assert!((r16_10.scale_ui - 1.0).abs() < 1e-9);

        // Critical round_ties_even parity check: 15 * (1600/1920) = 12.5 -> 12
        let r4_3 = compute_ui_layout(1600, 1200, false).expect("4:3");
        assert_eq!(r4_3.aspect_label, AspectLabel::Ratio4x3);
        assert_eq!(r4_3.canvas_w, 1920);
        assert_eq!(r4_3.canvas_h, 1440);
        assert_eq!(
            r4_3.minimap_rect,
            RectI32 {
                x: 49,
                y: 12,
                width: 182,
                height: 182
            }
        );

        let uw32 = compute_ui_layout(5120, 1440, false).expect("32:9");
        assert_eq!(uw32.aspect_label, AspectLabel::Ratio32x9);
        assert_eq!(uw32.canvas_w, 3840);
        assert_eq!(uw32.canvas_h, 1080);

        assert!(compute_ui_layout(0, 1080, false).is_err());
    }

    #[test]
    fn compute_ui_layout_16x9_and_narrower_bit_identical() {
        let expect_crop_covers = |lay: &UiLayout| -> (i32, i32) {
            let crop = compute_tracker_hud_crop(lay);
            assert!(crop.0 <= lay.viewport_w as i32 && crop.1 <= lay.viewport_h as i32);
            assert!(crop.0 >= lay.minimap_rect.x + lay.minimap_rect.width);
            assert!(crop.1 >= lay.minimap_rect.y + lay.minimap_rect.height);
            assert!(crop.0 >= lay.paimon_maybe_rect.x + lay.paimon_maybe_rect.width);
            crop
        };

        // 1920x1080 desktop & gamepad (exact baseline)
        let d1080 = compute_ui_layout(1920, 1080, false).unwrap();
        assert_eq!(d1080.canvas_w, 1920);
        assert_eq!(d1080.canvas_h, 1080);
        assert_eq!(d1080.minimap_rect_canvas, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(d1080.minimap_rect, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(d1080.paimon_rect_canvas, PAIMON_RECT_DESKTOP_1080P);
        assert_eq!(d1080.paimon_rect, PAIMON_RECT_DESKTOP_1080P);
        assert_eq!(
            d1080.paimon_maybe_rect,
            RectI32 {
                x: 0,
                y: 0,
                width: 192,
                height: 108
            }
        );
        assert_eq!(
            d1080.minimap_maybe_rect,
            RectI32 {
                x: 0,
                y: 0,
                width: 345,
                height: 237
            }
        );
        assert_eq!(expect_crop_covers(&d1080), (325, 261));

        let g1080 = compute_ui_layout(1920, 1080, true).unwrap();
        assert_eq!(g1080.minimap_rect, MINIMAP_RECT_GAMEPAD_1080P);
        assert_eq!(g1080.paimon_rect, PAIMON_RECT_GAMEPAD_1080P);
        assert_eq!(g1080.paimon_maybe_rect, d1080.paimon_maybe_rect);
        assert_eq!(g1080.minimap_maybe_rect, d1080.minimap_maybe_rect);
        assert_eq!(expect_crop_covers(&g1080), (347, 255));

        // 2560x1440 desktop & gamepad
        let d1440 = compute_ui_layout(2560, 1440, false).unwrap();
        assert_eq!(d1440.canvas_w, 1920);
        assert_eq!(d1440.canvas_h, 1080);
        assert_eq!(
            d1440.minimap_rect,
            RectI32 {
                x: 79,
                y: 20,
                width: 291,
                height: 291
            }
        );
        assert_eq!(
            d1440.paimon_rect,
            RectI32 {
                x: 33,
                y: 16,
                width: 91,
                height: 103
            }
        );
        assert_eq!(
            d1440.paimon_maybe_rect,
            RectI32 {
                x: 0,
                y: 0,
                width: 256,
                height: 144
            }
        );
        assert_eq!(
            d1440.minimap_maybe_rect,
            RectI32 {
                x: 0,
                y: 0,
                width: 460,
                height: 316
            }
        );
        assert_eq!(expect_crop_covers(&d1440), (434, 348));

        let g1440 = compute_ui_layout(2560, 1440, true).unwrap();
        assert_eq!(
            g1440.minimap_rect,
            RectI32 {
                x: 156,
                y: 60,
                width: 243,
                height: 243
            }
        );
        assert_eq!(
            g1440.paimon_rect,
            RectI32 {
                x: 119,
                y: 56,
                width: 76,
                height: 85
            }
        );
        assert_eq!(expect_crop_covers(&g1440), (463, 340));

        // 3840x2160 (4K 16:9)
        let d4k = compute_ui_layout(3840, 2160, false).unwrap();
        assert_eq!(
            d4k.minimap_rect,
            RectI32 {
                x: 118,
                y: 30,
                width: 436,
                height: 436
            }
        );
        assert_eq!(
            d4k.paimon_rect,
            RectI32 {
                x: 50,
                y: 24,
                width: 136,
                height: 154
            }
        );
        assert_eq!(expect_crop_covers(&d4k), (650, 522));

        // 1366x768 (aspect ~1.7786 slightly > 16:9, extra canvas width 0.9375 -> inset rounds to 0)
        let d768 = compute_ui_layout(1366, 768, false).unwrap();
        assert_eq!(d768.minimap_rect_canvas, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(d768.paimon_rect_canvas, PAIMON_RECT_DESKTOP_1080P);
        assert_eq!(d768.paimon_maybe_rect.x, 0);
        assert_eq!(d768.minimap_maybe_rect.x, 0);
        expect_crop_covers(&d768);

        // 1920x1200 (16:10 desktop & gamepad)
        let d16_10 = compute_ui_layout(1920, 1200, false).unwrap();
        assert_eq!(d16_10.minimap_rect, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(d16_10.paimon_rect, PAIMON_RECT_DESKTOP_1080P);
        assert_eq!(
            d16_10.paimon_maybe_rect,
            RectI32 {
                x: 0,
                y: 0,
                width: 192,
                height: 120
            }
        );
        assert_eq!(
            d16_10.minimap_maybe_rect,
            RectI32 {
                x: 0,
                y: 0,
                width: 345,
                height: 264
            }
        );
        assert_eq!(expect_crop_covers(&d16_10), (325, 261));

        let g16_10 = compute_ui_layout(1920, 1200, true).unwrap();
        assert_eq!(g16_10.minimap_rect, MINIMAP_RECT_GAMEPAD_1080P);
        assert_eq!(g16_10.paimon_rect, PAIMON_RECT_GAMEPAD_1080P);
        expect_crop_covers(&g16_10);

        // 2560x1600 (16:10 HiDPI)
        let d2560_1600 = compute_ui_layout(2560, 1600, false).unwrap();
        assert_eq!(d2560_1600.aspect_label, AspectLabel::Ratio16x10);
        assert_eq!(d2560_1600.minimap_rect_canvas, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(d2560_1600.paimon_rect_canvas, PAIMON_RECT_DESKTOP_1080P);
        assert_eq!(d2560_1600.paimon_maybe_rect.x, 0);
        expect_crop_covers(&d2560_1600);

        // 1600x1200 (4:3)
        let d4_3 = compute_ui_layout(1600, 1200, false).unwrap();
        assert_eq!(
            d4_3.minimap_rect,
            RectI32 {
                x: 49,
                y: 12,
                width: 182,
                height: 182
            }
        );
        assert_eq!(
            d4_3.paimon_rect,
            RectI32 {
                x: 21,
                y: 10,
                width: 57,
                height: 64
            }
        );
        assert_eq!(expect_crop_covers(&d4_3), (271, 217));

        // 1080x1920 (tall)
        let tall = compute_ui_layout(1080, 1920, false).unwrap();
        assert_eq!(tall.aspect_label, AspectLabel::Tall);
        assert_eq!(tall.minimap_rect_canvas, MINIMAP_RECT_DESKTOP_1080P);
        assert_eq!(tall.paimon_rect_canvas, PAIMON_RECT_DESKTOP_1080P);
        assert_eq!(tall.paimon_maybe_rect.x, 0);
        expect_crop_covers(&tall);
    }

    #[test]
    fn compute_ui_layout_21x9_desktop_and_gamepad() {
        let assert_rect_inside = |inner: RectI32, outer: RectI32, label: &str| {
            assert!(
                inner.x >= outer.x
                    && inner.y >= outer.y
                    && inner.x + inner.width <= outer.x + outer.width
                    && inner.y + inner.height <= outer.y + outer.height,
                "{label}: {inner:?} not inside {outer:?}"
            );
        };

        // 2560x1080 desktop (measured +72 px X shift)
        let d2560 = compute_ui_layout(2560, 1080, false).unwrap();
        assert_eq!(d2560.aspect_label, AspectLabel::Ratio21x9);
        assert_eq!(
            d2560.minimap_rect,
            RectI32 {
                x: 131,
                y: 15,
                width: 218,
                height: 218
            }
        );
        assert_eq!(
            d2560.paimon_rect,
            RectI32 {
                x: 97,
                y: 12,
                width: 68,
                height: 77
            }
        );
        assert_eq!(
            d2560.paimon_maybe_rect,
            RectI32 {
                x: 72,
                y: 0,
                width: 192,
                height: 108
            }
        );
        assert_eq!(
            d2560.minimap_maybe_rect,
            RectI32 {
                x: 72,
                y: 0,
                width: 345,
                height: 237
            }
        );
        assert_rect_inside(d2560.paimon_rect, d2560.paimon_maybe_rect, "2560x1080 desktop paimon");
        assert_rect_inside(d2560.minimap_rect, d2560.minimap_maybe_rect, "2560x1080 desktop minimap");
        let crop_d2560 = compute_tracker_hud_crop(&d2560);
        assert_eq!(crop_d2560, (397, 261));
        // True minimap circle on 2560x1080 capture: cx=241.0, cy=124.0, r=106.5
        assert!(241.0 - 106.5 >= d2560.minimap_rect.x as f64);
        assert!(241.0 + 106.5 <= (d2560.minimap_rect.x + d2560.minimap_rect.width) as f64);
        assert!(241.0 + 106.5 <= crop_d2560.0 as f64);
        assert!(124.0 + 106.5 <= crop_d2560.1 as f64);

        // 2560x1080 gamepad
        let g2560 = compute_ui_layout(2560, 1080, true).unwrap();
        assert_eq!(
            g2560.minimap_rect,
            RectI32 {
                x: 189,
                y: 45,
                width: 182,
                height: 182
            }
        );
        assert_eq!(
            g2560.paimon_rect,
            RectI32 {
                x: 161,
                y: 42,
                width: 57,
                height: 64
            }
        );
        assert_rect_inside(g2560.paimon_rect, g2560.paimon_maybe_rect, "2560x1080 gamepad paimon");
        assert_rect_inside(g2560.minimap_rect, g2560.minimap_maybe_rect, "2560x1080 gamepad minimap");
        assert_eq!(compute_tracker_hud_crop(&g2560), (419, 255));

        // 3440x1440 desktop (extrapolated inset_x_canvas = round_ties_even((2580 - 1920) * 0.1125) = 74)
        let d3440 = compute_ui_layout(3440, 1440, false).unwrap();
        assert_eq!(d3440.aspect_label, AspectLabel::Ratio21x9);
        assert_eq!(
            d3440.minimap_rect_canvas,
            RectI32 {
                x: 133,
                y: 15,
                width: 218,
                height: 218
            }
        );
        assert_eq!(
            d3440.minimap_rect,
            RectI32 {
                x: 177,
                y: 20,
                width: 291,
                height: 291
            }
        );
        assert_eq!(
            d3440.paimon_rect_canvas,
            RectI32 {
                x: 99,
                y: 12,
                width: 68,
                height: 77
            }
        );
        assert_eq!(
            d3440.paimon_rect,
            RectI32 {
                x: 132,
                y: 16,
                width: 91,
                height: 103
            }
        );
        assert_eq!(
            d3440.paimon_maybe_rect,
            RectI32 {
                x: 99,
                y: 0,
                width: 256,
                height: 144
            }
        );
        assert_eq!(
            d3440.minimap_maybe_rect,
            RectI32 {
                x: 99,
                y: 0,
                width: 460,
                height: 316
            }
        );
        assert_rect_inside(d3440.paimon_rect, d3440.paimon_maybe_rect, "3440x1440 desktop paimon");
        assert_rect_inside(d3440.minimap_rect, d3440.minimap_maybe_rect, "3440x1440 desktop minimap");
        assert_eq!(compute_tracker_hud_crop(&d3440), (532, 348));

        // 3440x1440 gamepad
        let g3440 = compute_ui_layout(3440, 1440, true).unwrap();
        assert_eq!(
            g3440.minimap_rect,
            RectI32 {
                x: 255,
                y: 60,
                width: 243,
                height: 243
            }
        );
        assert_eq!(
            g3440.paimon_rect,
            RectI32 {
                x: 217,
                y: 56,
                width: 76,
                height: 85
            }
        );
        assert_rect_inside(g3440.paimon_rect, g3440.paimon_maybe_rect, "3440x1440 gamepad paimon");
        assert_rect_inside(g3440.minimap_rect, g3440.minimap_maybe_rect, "3440x1440 gamepad minimap");
        assert_eq!(compute_tracker_hud_crop(&g3440), (562, 340));
    }

    #[test]
    fn compute_ui_layout_32x9_and_extreme_ultrawide_stay_in_bounds() {
        // Note: 32:9 and wider have no real capture yet; exact HUD placement is unverified.
        // This test guarantees non-panicking finite geometry strictly within frame bounds.
        let check_in_bounds = |r: RectI32, w: i32, h: i32, name: &str| {
            assert!(
                r.x >= 0 && r.y >= 0 && r.width > 0 && r.height > 0 && r.x + r.width <= w && r.y + r.height <= h,
                "{name} out of bounds for {w}x{h}: {r:?}"
            );
        };
        let assert_rect_inside = |inner: RectI32, outer: RectI32, name: &str| {
            assert!(
                inner.x >= outer.x
                    && inner.y >= outer.y
                    && inner.x + inner.width <= outer.x + outer.width
                    && inner.y + inner.height <= outer.y + outer.height,
                "{name}: {inner:?} not inside {outer:?}"
            );
        };

        for (w, h) in [(5120_u32, 1440_u32), (3840, 1080), (7680, 1080)] {
            for is_gp in [false, true] {
                let lay = compute_ui_layout(w, h, is_gp).expect("ultrawide layout");
                let wi = w as i32;
                let hi = h as i32;
                check_in_bounds(lay.minimap_rect, wi, hi, "minimap_rect");
                check_in_bounds(lay.paimon_rect, wi, hi, "paimon_rect");
                check_in_bounds(lay.minimap_maybe_rect, wi, hi, "minimap_maybe_rect");
                check_in_bounds(lay.paimon_maybe_rect, wi, hi, "paimon_maybe_rect");
                check_in_bounds(lay.uid_rect, wi, hi, "uid_rect");
                check_in_bounds(lay.uid_maybe_rect, wi, hi, "uid_maybe_rect");

                assert_rect_inside(lay.paimon_rect, lay.paimon_maybe_rect, "paimon in maybe");
                assert_rect_inside(lay.minimap_rect, lay.minimap_maybe_rect, "minimap in maybe");

                let (crop_w, crop_h) = compute_tracker_hud_crop(&lay);
                assert!(crop_w > 0 && crop_w <= wi && crop_h > 0 && crop_h <= hi);
                assert!(crop_w >= lay.minimap_rect.x + lay.minimap_rect.width);
                assert!(crop_h >= lay.minimap_rect.y + lay.minimap_rect.height);
                assert!(crop_w >= lay.paimon_maybe_rect.x + lay.paimon_maybe_rect.width);
                assert!(crop_h >= lay.paimon_maybe_rect.y + lay.paimon_maybe_rect.height);
            }
        }
    }

    #[test]
    fn hdr_clipping_detector_handles_empty_normal_snow_and_clipped_frames() {
        let empty = FrameView {
            width: 0,
            height: 0,
            stride: 0,
            format: PixelFormat::Bgr8,
            data: &[],
        };
        let r_empty = detect_hdr_clipping(empty);
        assert!(!r_empty.clipped);
        assert_eq!(r_empty.p95, 0.0);
        assert_eq!(r_empty.white_ratio, 0.0);

        let mut normal = FrameBuf::new_zeroed(10, 10, PixelFormat::Gray8);
        normal.data.fill(110);
        assert!(!detect_hdr_clipping(normal.as_view()).clipped);

        let mut snow = FrameBuf::new_zeroed(10, 10, PixelFormat::Gray8);
        snow.data.fill(215);
        snow.data[0..10].fill(255);
        let r_snow = detect_hdr_clipping(snow.as_view());
        assert!(!r_snow.clipped);
        assert!((r_snow.white_ratio - 0.10).abs() < 1e-6);

        let mut washed = FrameBuf::new_zeroed(10, 10, PixelFormat::Bgra8);
        for (idx, px) in washed.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let val = if idx < 60 { 254 } else { 100 };
            px[0] = val;
            px[1] = val;
            px[2] = val;
            px[3] = 255;
        }
        let r_washed = detect_hdr_clipping(washed.as_view());
        assert!(r_washed.clipped);
        assert!(r_washed.p95 >= 250.0);
        assert!((r_washed.white_ratio - 0.60).abs() < 1e-6);

        // OpenCV cv2.cvtColor(BGR2GRAY) 15-bit rounding parity check:
        // BGR (246, 254, 242) -> 0.114*246 + 0.587*254 + 0.299*242 = 249.500 -> 250 in OpenCV.
        let mut boundary = FrameBuf::new_zeroed(100, 100, PixelFormat::Bgr8);
        for px in boundary.data.as_chunks_mut::<3>().0 {
            px[0] = 246;
            px[1] = 254;
            px[2] = 242;
        }
        let r_boundary = detect_hdr_clipping(boundary.as_view());
        assert!(r_boundary.clipped);
        assert_eq!(r_boundary.p95, 250.0);
        assert_eq!(r_boundary.white_ratio, 1.0);
    }

    #[test]
    fn borderless_launch_args_order_and_validation() {
        let bare = build_borderless_launch_args(2560, 1440, None).expect("bare args");
        assert_eq!(
            bare,
            vec![
                "-popupwindow",
                "-screen-fullscreen",
                "0",
                "-screen-width",
                "2560",
                "-screen-height",
                "1440"
            ]
        );

        let with_exe = build_borderless_launch_args(
            1920,
            1080,
            Some(r"D:\Genshin\GenshinImpact.exe"),
        )
        .expect("with exe");
        assert_eq!(with_exe[0], r"D:\Genshin\GenshinImpact.exe");
        assert_eq!(with_exe[1], "-popupwindow");

        assert!(build_borderless_launch_args(0, 1080, None).is_err());
    }
}
