//! Diamond-masked NCC zoom slider detector + linear scale law (`src/zoom_slider.py`).

use crate::image_io::{ImageReadMode, decode_image_bytes};
use crate::ops::color::{bgr_to_gray, frame_view_to_bgr};
use crate::ops::morph::draw_filled_convex_poly_u8;
use crate::ops::ncc::{
    PreparedTemplateFft, match_template_u8_ccoeff_prepared_argmax,
    prepare_ccoeff_template_fft_u8,
};
use crate::ops::resize::resize_u8_hwc;
use crate::ops::{Interpolation, Point2i};
use gh_core::frame::FrameView;
use ndarray::{Array2, Array3, ArrayView2, ArrayView3, s};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub const RHO_MIN: f64 = 0.1250;
pub const RHO_MAX: f64 = 0.8750;
pub const RHO_SPAN: f64 = 0.7500;

pub const SCALE_T0_1440P: f64 = 1.9242;
pub const SCALE_T1_1440P: f64 = 0.3186;

pub const MODEL_A: f64 = 0.0;
pub const MODEL_B: f64 = -1.6056;
pub const MODEL_C: f64 = 1.9242;

pub const SLIDER_MIN_CCORR: f64 = 0.88;

/// Map 40 (Nod-Krai / Frost Moon) linear scale law @2560x1440 (`s_40(t) = MID - SLOPE * (t - 0.50)`).
/// Calibrated via OLS across 7 distinct zoom levels (11 frames from `sess_20261127_210311`,
/// `sess_20261127_011544`, `sess_20261127_081807`, and `Test-images/Screenshot 2026-09-20 102751.png`,
/// verified by both isometric waypoint LSQ and Sobel terrain correlation peaks):
///   t=0.4323 -> 1.0191 (old 1.0396, +2.0%), t=0.4985 -> 0.9220 (+1.7%),
///   t=0.5313 -> 0.8707 (+1.8%), t=0.5833 -> 0.7982 (+1.0%),
///   t=0.6823 -> 0.6476 (+0.9%), t=0.7448 -> 0.5617 (-0.9%), t=0.7917 -> 0.4874 (-0.6%).
/// Max relative residual across t in [0.43, 0.79]: 0.62% (<= 0.26% across [0.43, 0.59]).
pub const MAP40_SCALE_MID_1440P: f64 = 0.919;
pub const MAP40_SCALE_SLOPE_1440P: f64 = 1.474;
pub const MAP40_SCALE_MIN_1440P: f64 = 0.18;
pub const MAP40_SCALE_MAX_1440P: f64 = 1.66;

const ZOOM_THUMB_TEMPLATE_PNG: &[u8] = include_bytes!("../../../../assets/zoom_thumb_template.png");

/// Per-map scale multipliers relative to Map 2 (Teyvat) at the same zoom slider ratio `t`
/// (`src/zoom_slider.py::MAP_SCALE_MULTIPLIERS`).
pub const fn map_scale_multiplier(map_id: i32) -> f64 {
    match map_id {
        2 => 1.000,
        40 => 0.810,
        36 => 0.728,
        34 => 0.713,
        9 => 0.762,
        7 => 0.620,
        37 => 0.713,
        _ => 0.75,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoomSliderResult {
    pub success: bool,
    pub found: bool,
    pub ratio_t: f64,
    pub rho: f64,
    pub scale: f64,
    pub match_score: f64,
    pub score: f64,
    pub ccoeff: f64,
    pub box_coords: [i32; 4],
    pub x_track: f64,
    pub y_thumb_global: f64,
    pub total_span: f64,
}

type PreparedFftCache = Arc<Mutex<HashMap<(usize, usize), Arc<PreparedTemplateFft>>>>;

#[derive(Debug, Clone)]
pub(crate) struct CachedDiamondTpl {
    pub tpl_bgr: Array3<u8>,
    pub mask: Array2<u8>,
    pub tpl_gray: Array2<u8>,
    fft_cache: PreparedFftCache,
}

impl CachedDiamondTpl {
    fn get_or_prepare_fft(&self, ih: usize, iw: usize) -> Option<Arc<PreparedTemplateFft>> {
        let key = (ih, iw);
        if let Ok(guard) = self.fft_cache.lock()
            && let Some(hit) = guard.get(&key)
        {
            return Some(Arc::clone(hit));
        }
        let prep = prepare_ccoeff_template_fft_u8(self.tpl_gray.view(), ih, iw).ok()?;
        if let Ok(mut guard) = self.fft_cache.lock() {
            guard.insert(key, Arc::clone(&prep));
        }
        Some(prep)
    }
}

fn synthetic_fallback_template() -> Array3<u8> {
    let mut img = Array3::<u8>::from_elem((31, 31, 3), 83u8);
    for y in 0..31isize {
        for x in 0..31isize {
            let d = (((x - 15) * (x - 15) + (y - 15) * (y - 15)) as f64).sqrt();
            if (12.0..=16.0).contains(&d) {
                img[[y as usize, x as usize, 0]] = 230;
                img[[y as usize, x as usize, 1]] = 230;
                img[[y as usize, x as usize, 2]] = 230;
            }
        }
    }
    img
}

fn base_template() -> &'static Array3<u8> {
    static BASE: OnceLock<Array3<u8>> = OnceLock::new();
    BASE.get_or_init(|| {
        decode_image_bytes(ZOOM_THUMB_TEMPLATE_PNG, ImageReadMode::Color)
            .and_then(|d| d.into_bgr())
            .unwrap_or_else(|_| synthetic_fallback_template())
    })
}

pub(crate) fn get_diamond_template_and_mask(
    target_h: usize,
    target_w: usize,
) -> Arc<CachedDiamondTpl> {
    type DiamondCache = OnceLock<Mutex<HashMap<(usize, usize), Arc<CachedDiamondTpl>>>>;
    static CACHE: DiamondCache = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (target_h, target_w);
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(&key)
    {
        return Arc::clone(hit);
    }

    let base = base_template();
    let tpl_bgr = resize_u8_hwc(base.view(), target_w, target_h, Interpolation::Area)
        .unwrap_or_else(|_| base.clone());
    let mut mask = Array2::<u8>::zeros((target_h, target_w));
    let pts = [
        Point2i {
            x: (target_w / 2) as i32,
            y: 0,
        },
        Point2i {
            x: (target_w - 1) as i32,
            y: (target_h / 2) as i32,
        },
        Point2i {
            x: (target_w / 2) as i32,
            y: (target_h - 1) as i32,
        },
        Point2i {
            x: 0,
            y: (target_h / 2) as i32,
        },
    ];
    draw_filled_convex_poly_u8(&mut mask, &pts, 255);
    let tpl_gray = bgr_to_gray(tpl_bgr.view()).expect("3-ch template bgr_to_gray");
    let item = Arc::new(CachedDiamondTpl {
        tpl_bgr,
        mask,
        tpl_gray,
        fft_cache: Arc::new(Mutex::new(HashMap::new())),
    });
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, Arc::clone(&item));
    }
    item
}

/// Evaluates masked BGR `TM_CCORR_NORMED` and `TM_CCOEFF_NORMED` at a single patch location
/// using the global scalar mean across all `3 * N_m` masked channel samples (matching NumPy
/// `a - a.mean()` in `src/zoom_slider.py:113-123`).
pub(crate) fn masked_bgr_ccorr_and_ccoeff(
    patch: ArrayView3<'_, u8>,
    tpl: ArrayView3<'_, u8>,
    mask: ArrayView2<'_, u8>,
) -> (f64, f64) {
    let (h, w, c) = tpl.dim();
    if patch.dim() != (h, w, c) || mask.dim() != (h, w) || c < 3 {
        return (0.0, 0.0);
    }
    let mut count = 0usize;
    let mut sum_a = 0.0f64;
    let mut sum_b = 0.0f64;
    let mut sum_aa = 0.0f64;
    let mut sum_bb = 0.0f64;
    let mut sum_ab = 0.0f64;

    for y in 0..h {
        for x in 0..w {
            if mask[[y, x]] == 0 {
                continue;
            }
            for ch in 0..3 {
                let va = patch[[y, x, ch]] as f64;
                let vb = tpl[[y, x, ch]] as f64;
                sum_a += va;
                sum_b += vb;
                sum_aa += va * va;
                sum_bb += vb * vb;
                sum_ab += va * vb;
                count += 1;
            }
        }
    }
    if count == 0 {
        return (0.0, 0.0);
    }
    let na = sum_aa.sqrt();
    let nb = sum_bb.sqrt();
    let ccorr = if na > 1e-3 && nb > 1e-3 {
        sum_ab / (na * nb)
    } else {
        0.0
    };

    let n_f = count as f64;
    let mean_a = sum_a / n_f;
    let mean_b = sum_b / n_f;
    let mut ac_sq = 0.0f64;
    let mut bc_sq = 0.0f64;
    let mut ab_c = 0.0f64;
    for y in 0..h {
        for x in 0..w {
            if mask[[y, x]] == 0 {
                continue;
            }
            for ch in 0..3 {
                let da = patch[[y, x, ch]] as f64 - mean_a;
                let db = tpl[[y, x, ch]] as f64 - mean_b;
                ac_sq += da * da;
                bc_sq += db * db;
                ab_c += da * db;
            }
        }
    }
    let nac = ac_sq.sqrt();
    let nbc = bc_sq.sqrt();
    let ccoeff = if nac > 1e-3 && nbc > 1e-3 {
        ab_c / (nac * nbc)
    } else {
        0.0
    };
    (ccorr, ccoeff)
}

pub struct ZoomSliderDetector;

impl ZoomSliderDetector {
    pub fn detect_in_roi(
        roi: ArrayView3<'_, u8>,
        full_w: usize,
        full_h: usize,
        x_start: usize,
        y_start: usize,
    ) -> Option<ZoomSliderResult> {
        let (roi_h, roi_w, channels) = roi.dim();
        if roi_h == 0 || roi_w == 0 || channels < 3 || full_h == 0 {
            return None;
        }
        let scale_ui = full_h as f64 / 1440.0;
        let fw_ratio = 2560.0 / (full_w.max(1) as f64);

        let tpl_w = ((31.0 * scale_ui).round_ties_even() as isize).max(15) as usize;
        let tpl_h = ((31.0 * scale_ui).round_ties_even() as isize).max(15) as usize;
        if roi_h < tpl_h || roi_w < tpl_w {
            return None;
        }

        let roi_bgr = if channels > 3 {
            roi.slice(s![.., .., 0..3])
        } else {
            roi
        };
        let cached = get_diamond_template_and_mask(tpl_h, tpl_w);
        let roi_gray = bgr_to_gray(roi_bgr).ok()?;
        let (gi_h, gi_w) = roi_gray.dim();
        let prep = cached.get_or_prepare_fft(gi_h, gi_w)?;
        let (_, (px0, py0)) = match_template_u8_ccoeff_prepared_argmax(roi_gray.view(), &prep).ok()?;

        let patch = roi_bgr.slice(s![py0..py0 + tpl_h, px0..px0 + tpl_w, ..]);
        let (ccorr, ccoeff) =
            masked_bgr_ccorr_and_ccoeff(patch, cached.tpl_bgr.view(), cached.mask.view());
        if ccorr < SLIDER_MIN_CCORR {
            return None;
        }

        let thumb_cx = x_start as f64 + px0 as f64 + (tpl_w as f64 / 2.0);
        let thumb_cy = py0 as f64 + (tpl_h as f64 / 2.0);

        let y_minus_roi = 531.5 * scale_ui;
        let total_span = 256.0 * scale_ui;
        let rho = (y_minus_roi - thumb_cy) / total_span;
        let ratio_t = ((rho - RHO_MIN) / RHO_SPAN).clamp(0.0, 1.0);

        let scale_1440p = SCALE_T0_1440P + (SCALE_T1_1440P - SCALE_T0_1440P) * ratio_t;
        let scale_current = scale_1440p * fw_ratio;

        let box_coords = [
            (thumb_cx - 22.0 * scale_ui).round_ties_even() as i32,
            (y_start as f64 + 255.0 * scale_ui).round_ties_even() as i32,
            (thumb_cx + 22.0 * scale_ui).round_ties_even() as i32,
            (y_start as f64 + 552.0 * scale_ui).round_ties_even() as i32,
        ];

        Some(ZoomSliderResult {
            success: true,
            found: true,
            ratio_t,
            rho,
            scale: scale_current,
            match_score: ccorr,
            score: ccorr,
            ccoeff,
            box_coords,
            x_track: thumb_cx,
            y_thumb_global: y_start as f64 + thumb_cy,
            total_span,
        })
    }

    pub fn detect_slider(frame_bgr: ArrayView3<'_, u8>) -> Option<ZoomSliderResult> {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 {
            return None;
        }
        let x_start = (w as f64 * 0.012) as usize;
        let x_end = w.min((w as f64 * 0.062) as usize);
        let y_start = (h as f64 * 0.22) as usize;
        let y_end = h.min((h as f64 * 0.68) as usize);
        if x_end <= x_start || y_end <= y_start {
            return None;
        }
        let roi = frame_bgr.slice(s![y_start..y_end, x_start..x_end, ..]);
        Self::detect_in_roi(roi, w, h, x_start, y_start)
    }

    pub fn detect_slider_frame(frame: FrameView<'_>) -> Option<ZoomSliderResult> {
        let bgr = frame_view_to_bgr(frame).ok()?;
        Self::detect_slider(bgr.view())
    }

    pub fn get_map_scale(
        slider_res: Option<&ZoomSliderResult>,
        map_id: i32,
        frame_w: usize,
    ) -> Option<f64> {
        let res = slider_res?;
        if !res.ratio_t.is_finite() {
            return None;
        }
        let t = res.ratio_t.clamp(0.0, 1.0);
        let fw_ratio = 2560.0 / (frame_w.max(1) as f64);
        if map_id == 40 {
            let s_40 = (MAP40_SCALE_MID_1440P - MAP40_SCALE_SLOPE_1440P * (t - 0.50))
                .clamp(MAP40_SCALE_MIN_1440P, MAP40_SCALE_MAX_1440P);
            return Some(s_40 * fw_ratio);
        }
        let s_m2 = (SCALE_T0_1440P + (SCALE_T1_1440P - SCALE_T0_1440P) * t) * fw_ratio;
        Some(s_m2 * map_scale_multiplier(map_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_map_scale_matches_python_constants() {
        let make_res = |t: f64| ZoomSliderResult {
            success: true,
            found: true,
            ratio_t: t,
            rho: RHO_MIN + t * RHO_SPAN,
            scale: 0.0,
            match_score: 1.0,
            score: 1.0,
            ccoeff: 1.0,
            box_coords: [0; 4],
            x_track: 0.0,
            y_thumb_global: 0.0,
            total_span: 256.0,
        };

        for (t, exp_m2, exp_40, exp_36, exp_999) in [
            (0.0, 1.9242, 1.656, 1.400_817_6, 1.443_15),
            (0.5, 1.1214, 0.919, 0.816_379_2, 0.841_05),
            (1.0, 0.3186, 0.182, 0.231_940_8, 0.238_95),
        ] {
            let r = make_res(t);
            assert!((ZoomSliderDetector::get_map_scale(Some(&r), 2, 2560).unwrap() - exp_m2).abs() < 1e-6);
            assert!((ZoomSliderDetector::get_map_scale(Some(&r), 40, 2560).unwrap() - exp_40).abs() < 1e-6);
            assert!((ZoomSliderDetector::get_map_scale(Some(&r), 36, 2560).unwrap() - exp_36).abs() < 1e-6);
            assert!((ZoomSliderDetector::get_map_scale(Some(&r), 999, 2560).unwrap() - exp_999).abs() < 1e-6);
        }

        // 1080p width scaling (2560 / 1920 = 4/3)
        let r_mid = make_res(0.5);
        let s_1080 = ZoomSliderDetector::get_map_scale(Some(&r_mid), 2, 1920).unwrap();
        assert!((s_1080 - 1.1214 * (2560.0 / 1920.0)).abs() < 1e-6);
        assert!(ZoomSliderDetector::get_map_scale(None, 2, 2560).is_none());
    }

    #[test]
    fn flat_bright_frame_rejected_by_ccoeff_or_ccorr() {
        let flat = Array3::<u8>::from_elem((1440, 2560, 3), 235u8);
        if let Some(res) = ZoomSliderDetector::detect_slider(flat.view()) {
            assert!(res.ccoeff < 0.65, "flat panel ccoeff={}", res.ccoeff);
        }
    }

    #[test]
    fn embedded_template_synthetic_frame_roundtrip() {
        // Place the real embedded 31x31 thumb into a synthetic 2560x1440 dark slider track
        // at t=0.5 (rho = 0.5 -> thumb_cy = 531.5 - 128.0 = 403.5 -> y_start=316, py0=72)
        let mut frame = Array3::<u8>::from_elem((1440, 2560, 3), 30u8);
        let tpl = base_template();
        assert_eq!(tpl.dim(), (31, 31, 3));
        let x0 = 48usize;
        let y_start = (1440.0 * 0.22) as usize; // 316
        let py0 = 72usize;
        let y0 = y_start + py0;
        for dy in 0..31 {
            for dx in 0..31 {
                for ch in 0..3 {
                    frame[[y0 + dy, x0 + dx, ch]] = tpl[[dy, dx, ch]];
                }
            }
        }
        let res = ZoomSliderDetector::detect_slider(frame.view()).expect("detect embedded template");
        assert!(res.match_score > 0.999);
        assert!(res.ccoeff > 0.999);
        assert!((res.y_thumb_global - (y0 as f64 + 15.5)).abs() < 1e-6);
    }

    #[test]
    fn map40_scale_matches_measured_calibration_points() {
        // 7 distinct Map 40 zoom levels @2560x1440 verified via waypoint LSQ & Sobel terrain peaks:
        // (t, measured_scale_1440p)
        let measured = [
            (0.432_292, 1.019_08),
            (0.498_540, 0.922_03),
            (0.531_250, 0.870_70),
            (0.583_333, 0.798_19),
            (0.682_292, 0.647_64),
            (0.744_792, 0.561_67),
            (0.791_667, 0.487_40),
        ];
        for (t, exp_s) in measured {
            let res = ZoomSliderResult {
                success: true,
                found: true,
                ratio_t: t,
                rho: RHO_MIN + t * RHO_SPAN,
                scale: 0.0,
                match_score: 1.0,
                score: 1.0,
                ccoeff: 1.0,
                box_coords: [0; 4],
                x_track: 0.0,
                y_thumb_global: 0.0,
                total_span: 256.0,
            };
            let got = ZoomSliderDetector::get_map_scale(Some(&res), 40, 2560)
                .expect("map 40 scale");
            let rel_err = (got - exp_s).abs() / exp_s;
            assert!(
                rel_err <= 0.01,
                "t={t:.6}: got={got:.5}, expected={exp_s:.5} (rel_err={:.3}%)",
                rel_err * 100.0
            );
        }
    }
}
