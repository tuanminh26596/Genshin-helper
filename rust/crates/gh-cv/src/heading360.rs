//! 3-tier camera cone + 3-tip player arrow 360-degree heading estimator & `PureOpenCVTracker`
//! (`src/heading360.py` + `src/tracker.py`).

use crate::ops::color::{
    bgr_px_to_gray, bgr_to_hsv, frame_view_to_array3, in_range, swap_rb,
};
use crate::ops::contours::{
    connected_components_with_stats_8, contour_area, contour_moments, find_contours_external,
};
use crate::ops::morph::{
    draw_filled_convex_poly_u8, get_structuring_element, morphology_ex_u8,
};
use crate::ops::resize::{copy_make_border_f32, copy_make_border_u8, resize_u8_hwc};
use crate::ops::sobel::{scharr_dy_f32, scharr_dy_u8_f32};
use crate::ops::warp::{warp_polar_annulus_u8, warp_polar_annulus_u8_hwc};
use crate::ops::{
    BorderMode, ContourMethod, CvError, CvResult, Interpolation, MorphOp, MorphShape, Point2i,
};
use gh_core::frame::{FrameView, PixelFormat};
use ndarray::{Array2, Array3, ArrayView2, ArrayView3, s};
use std::sync::OnceLock;

pub const POLAR_BINS: usize = 512;
pub const RAD_BINS: usize = 128;
pub const RAD_R0: usize = 26;
pub const RAD_R1: usize = 102;
pub const CONE_BLEND_ALPHA: f64 = 0.25;
pub const CONE_ROLL_SHIFT: usize = 5;
pub const CONE_SECTOR_BINS: usize = 64;
pub const CONE_SECTOR_RAD_SAMPLES: usize = 15;
pub type InnerConeSectorGrid = [[u8; CONE_SECTOR_RAD_SAMPLES]; CONE_SECTOR_BINS];
/// Mean-subtracted top-3 per-ray-median inner-cone luminance diff threshold separating
/// stationary snowstorm/icon noise (`~0.0..1.6`) from real camera rotation (`~5.2..34.2`).
pub const CONE_STATIC_SECTOR_TOP3_THRESH: f64 = 4.8;

pub const CYAN_HSV_LOWER: [u8; 3] = [85, 160, 180];
pub const CYAN_HSV_UPPER: [u8; 3] = [105, 255, 255];
pub const YELLOW_PING_HSV_LOWER: [u8; 3] = [20, 120, 180];
pub const YELLOW_PING_HSV_UPPER: [u8; 3] = [35, 255, 255];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConeMethod {
    #[default]
    None,
    BgraAlpha,
    BgrBlend,
    HsvAdaptive,
}

impl ConeMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BgraAlpha => "bgra_alpha",
            Self::BgrBlend => "bgr_blend",
            Self::HsvAdaptive => "hsv_adaptive",
        }
    }

    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::BgraAlpha | Self::BgrBlend)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FuserSource {
    #[default]
    None,
    BgraAlpha,
    BgrBlend,
    HsvAdaptive,
    Body,
}

impl FuserSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BgraAlpha => "bgra_alpha",
            Self::BgrBlend => "bgr_blend",
            Self::HsvAdaptive => "hsv_adaptive",
            Self::Body => "body",
        }
    }
}

impl From<ConeMethod> for FuserSource {
    fn from(m: ConeMethod) -> Self {
        match m {
            ConeMethod::None => Self::None,
            ConeMethod::BgraAlpha => Self::BgraAlpha,
            ConeMethod::BgrBlend => Self::BgrBlend,
            ConeMethod::HsvAdaptive => Self::HsvAdaptive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ConeDetection {
    pub heading_deg: Option<f64>,
    pub confidence: f64,
    pub method: ConeMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ArrowDetection {
    pub heading_deg: Option<f64>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FullscreenPlayerPose {
    pub pixel_x: f64,
    pub pixel_y: f64,
    pub angle_deg: f64,
}

#[inline]
pub fn wrap_angle_360(deg: f64) -> f64 {
    if !deg.is_finite() {
        return 0.0;
    }
    let val = deg.rem_euclid(360.0);
    if val >= 360.0 || val == 0.0 {
        0.0
    } else {
        val
    }
}

#[inline]
pub fn shortest_angle_delta(from_deg: f64, to_deg: f64) -> f64 {
    let delta = (to_deg - from_deg + 180.0).rem_euclid(360.0) - 180.0;
    if delta == -180.0 {
        180.0
    } else if delta == 0.0 {
        0.0
    } else {
        delta
    }
}

pub fn compute_nav_relative_angle(
    player_pos: [f64; 2],
    target_pos: [f64; 2],
    view_deg_cw_north: f64,
) -> (f64, f64) {
    let dx = target_pos[0] - player_pos[0];
    let dy = target_pos[1] - player_pos[1];
    let dist = dx.hypot(dy);
    if dist < 1e-9 {
        return (0.0, 0.0);
    }
    let target_bearing_deg = wrap_angle_360(dx.atan2(-dy).to_degrees());
    let delta_deg = shortest_angle_delta(view_deg_cw_north, target_bearing_deg);
    (delta_deg, dist)
}

fn lut_pred_u8() -> &'static [u8; 256] {
    static LUT: OnceLock<[u8; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut out = [0u8; 256];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = ((i as f64 * 0.75 + 63.75).round_ties_even() as i32).clamp(0, 255) as u8;
        }
        out
    })
}

/// Linear-light 25% white-veil blend LUT for HDR/scRGB tonemapped minimap captures
/// (`WGC RGBA16F -> sRGB` where the game composites the camera cone in linear light).
fn lut_pred_linear_u8() -> &'static [u8; 256] {
    static LUT: OnceLock<[u8; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut out = [0u8; 256];
        for (i, slot) in out.iter_mut().enumerate() {
            let s = i as f64 / 255.0;
            let lin = if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            };
            let blended = (0.75 * lin + 0.25).clamp(0.0, 1.0);
            let srgb = if blended <= 0.0031308 {
                12.92 * blended
            } else {
                1.055 * blended.powf(1.0 / 2.4) - 0.055
            };
            *slot = ((srgb * 255.0).round_ties_even() as i32).clamp(0, 255) as u8;
        }
        out
    })
}

fn morph_ellipse_3x3() -> &'static Array2<u8> {
    static K: OnceLock<Array2<u8>> = OnceLock::new();
    K.get_or_init(|| get_structuring_element(MorphShape::Ellipse, 3, 3).expect("3x3 ellipse"))
}

/// Returns true when a 4-channel BGRA minimap frame carries a live non-opaque alpha mask
/// (Tier-1 GPU alpha cone path). Opaque BGRA (`A == 255`) and 3-channel BGR return false.
pub fn has_non_opaque_alpha(minimap_img: ArrayView3<'_, u8>) -> bool {
    if minimap_img.dim().2 != 4 {
        return false;
    }
    if let Some(s) = minimap_img.as_slice() {
        for px in s.as_chunks::<4>().0 {
            if px[3] != 255 {
                return true;
            }
        }
        false
    } else {
        minimap_img.slice(s![.., .., 3]).iter().any(|&a| a != 255)
    }
}

/// Samples a compact `64 × 15` (`960` bytes) inner-cone luminance grid over Genshin's real
/// active camera-cone band (`17%..55%` of minimap radius), excluding both the central player
/// arrow (`< 16%`) and outer translucent rim / pulsing edge icons / snow bleed (`> 60%`).
pub fn inner_cone_sector_profile(minimap_img: ArrayView3<'_, u8>) -> Option<InnerConeSectorGrid> {
    let (h, w, c) = minimap_img.dim();
    if (c != 3 && c != 4) || h.min(w) < 24 {
        return None;
    }
    let cx = w as f64 * 0.5;
    let cy = h as f64 * 0.5;
    let max_rad = (h.min(w) as f64) * 0.5;
    let w_max = (w - 1) as f64;
    let h_max = (h - 1) as f64;

    let mut rhos = [0.0f64; CONE_SECTOR_RAD_SAMPLES];
    for (k, dst) in rhos.iter_mut().enumerate() {
        // 15 evenly spaced radial bins spanning r_bin = 22..70 out of 128 (17.2%..54.7% R)
        let r_bin = 22.0 + (48.0 * k as f64) / (CONE_SECTOR_RAD_SAMPLES - 1) as f64;
        *dst = (r_bin / 128.0) * max_rad;
    }

    let mut out = [[0u8; CONE_SECTOR_RAD_SAMPLES]; CONE_SECTOR_BINS];
    let k_angle = (2.0 * std::f64::consts::PI) / CONE_SECTOR_BINS as f64;
    let contig;
    let s = if let Some(sl) = minimap_img.as_slice() {
        sl
    } else {
        contig = minimap_img.as_standard_layout().into_owned();
        contig.as_slice()?
    };
    let row_stride = w * c;

    for (sec, dst_ray) in out.iter_mut().enumerate() {
        let (sin_p, cos_p) = (sec as f64 * k_angle).sin_cos();
        for (k, &rho) in rhos.iter().enumerate() {
            let ix = (cx + rho * cos_p).round().clamp(0.0, w_max) as usize;
            let iy = (cy + rho * sin_p).round().clamp(0.0, h_max) as usize;
            let off = iy * row_stride + ix * c;
            dst_ray[k] = bgr_px_to_gray(s[off], s[off + 1], s[off + 2]);
        }
    }
    Some(out)
}

/// Computes the mean of the 3 largest mean-subtracted per-ray median signed luminance diffs.
/// Taking the median across the 15 radial pixels of each ray makes the metric completely immune
/// to isolated falling snowflakes of any brightness (up to 47% of a ray), while subtracting the
/// global mean cancels uniform time-of-day / cloud lighting drift and preserves coherent cone turns.
pub fn inner_cone_sector_top3_diff(
    curr: &InnerConeSectorGrid,
    reference: &InnerConeSectorGrid,
) -> f64 {
    let mut ray_meds = [0.0f32; CONE_SECTOR_BINS];
    let mut sum = 0.0f32;
    let mid = CONE_SECTOR_RAD_SAMPLES / 2;
    for sec in 0..CONE_SECTOR_BINS {
        let mut d_ray = [0i16; CONE_SECTOR_RAD_SAMPLES];
        for k in 0..CONE_SECTOR_RAD_SAMPLES {
            d_ray[k] = curr[sec][k] as i16 - reference[sec][k] as i16;
        }
        let (_, med, _) = d_ray.select_nth_unstable(mid);
        let m = *med as f32;
        ray_meds[sec] = m;
        sum += m;
    }
    let mean = sum / CONE_SECTOR_BINS as f32;
    let mut top = [0.0f32; 3];
    for m in ray_meds {
        let ad = (m - mean).abs();
        if ad > top[0] {
            top[2] = top[1];
            top[1] = top[0];
            top[0] = ad;
        } else if ad > top[1] {
            top[2] = top[1];
            top[1] = ad;
        } else if ad > top[2] {
            top[2] = ad;
        }
    }
    ((top[0] + top[1] + top[2]) / 3.0) as f64
}

#[inline]
fn circular_boxcar_5(vec: &[f32; POLAR_BINS]) -> [f32; POLAR_BINS] {
    let mut out = [0.0f32; POLAR_BINS];
    for i in 0..POLAR_BINS {
        let i_m2 = (i + POLAR_BINS - 2) % POLAR_BINS;
        let i_m1 = (i + POLAR_BINS - 1) % POLAR_BINS;
        let i_p1 = (i + 1) % POLAR_BINS;
        let i_p2 = (i + 2) % POLAR_BINS;
        out[i] = (vec[i_m2] + vec[i_m1] + vec[i] + vec[i_p1] + vec[i_p2]) * 0.2f32;
    }
    out
}

#[inline]
fn circular_subpixel_peak(arr: &[f32; POLAR_BINS], idx: usize, negate: bool) -> f64 {
    let sign = if negate { -1.0f64 } else { 1.0f64 };
    let y_prev = arr[(idx + POLAR_BINS - 1) % POLAR_BINS] as f64 * sign;
    let y_curr = arr[idx] as f64 * sign;
    let y_next = arr[(idx + 1) % POLAR_BINS] as f64 * sign;
    let denom = y_prev - 2.0 * y_curr + y_next;
    let offset = if denom.abs() > 1e-9 {
        (0.5 * (y_prev - y_next) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    (idx as f64 + offset).rem_euclid(POLAR_BINS as f64)
}

#[inline]
fn circular_bin_dist(a: usize, b: usize) -> usize {
    let d1 = (a + POLAR_BINS - b) % POLAR_BINS;
    let d2 = (b + POLAR_BINS - a) % POLAR_BINS;
    d1.min(d2)
}

fn wedge_brightness_ok(v_prof: &[f32; POLAR_BINS], center_bin: usize) -> bool {
    let mut in_sum = 0.0f64;
    let mut in_cnt = 0usize;
    let mut out_sum = 0.0f64;
    let mut out_cnt = 0usize;
    for (idx, &v) in v_prof.iter().enumerate() {
        let cd = circular_bin_dist(idx, center_bin);
        if cd <= 56 {
            in_sum += v as f64;
            in_cnt += 1;
        } else if (72..=112).contains(&cd) {
            out_sum += v as f64;
            out_cnt += 1;
        }
    }
    if in_cnt == 0 || out_cnt == 0 {
        return false;
    }
    let inside_v = in_sum / in_cnt as f64;
    let outside_v = out_sum / out_cnt as f64;
    inside_v > outside_v + 0.01
}

fn extract_cone_bgra_alpha_polar(alpha_channel: ArrayView2<'_, u8>) -> Option<(f64, f64)> {
    let mut a_min = 255u8;
    let mut a_max = 0u8;
    for &v in alpha_channel.iter() {
        a_min = a_min.min(v);
        a_max = a_max.max(v);
    }
    if a_min == 255 || a_max <= a_min {
        return None;
    }

    let (h, w) = alpha_channel.dim();
    let center = (w as f64 * 0.5, h as f64 * 0.5);
    let max_rad = (h.min(w) as f64) * 0.5;
    let ann_a = warp_polar_annulus_u8(
        alpha_channel,
        RAD_BINS,
        POLAR_BINS,
        (RAD_R0, RAD_R1),
        center,
        max_rad,
        Interpolation::Linear,
    )
    .ok()?;
    let h_rad = ann_a.dim().1;
    let padded = copy_make_border_u8(ann_a.view(), 2, 2, 0, 0, BorderMode::Wrap).ok()?;
    let grad_y = scharr_dy_u8_f32(padded.view()).ok()?;

    let mut row_sums = [0.0f32; POLAR_BINS];
    for (y, dst) in row_sums.iter_mut().enumerate() {
        let mut s = 0.0f32;
        for x in 0..h_rad {
            s += grad_y[[y + 2, x]];
        }
        *dst = s;
    }
    let col_sum = circular_boxcar_5(&row_sums);

    let mut min_idx = 0usize;
    let mut max_idx = 0usize;
    let mut min_val = f32::INFINITY;
    let mut max_val = f32::NEG_INFINITY;
    for (i, &v) in col_sum.iter().enumerate() {
        if v < min_val {
            min_val = v;
            min_idx = i;
        }
        if v > max_val {
            max_val = v;
            max_idx = i;
        }
    }

    let min_edge = 16.0 * (h_rad as f64) * 4.0;
    if (min_val as f64).abs() < min_edge || (max_val as f64).abs() < min_edge {
        return None;
    }

    let ang_min = (circular_subpixel_peak(&col_sum, min_idx, true) / POLAR_BINS as f64) * 360.0;
    let ang_max = (circular_subpixel_peak(&col_sum, max_idx, false) / POLAR_BINS as f64) * 360.0;
    let span_low = (ang_max - ang_min).rem_euclid(360.0);
    let span_high = (ang_min - ang_max).rem_euclid(360.0);

    let (mid_polar, span_err) = if (span_low - 90.0).abs() <= 15.0 {
        (ang_min + span_low * 0.5, (span_low - 90.0).abs())
    } else if (span_high - 90.0).abs() <= 15.0 {
        (ang_max + span_high * 0.5, (span_high - 90.0).abs())
    } else {
        return None;
    };

    let heading_cw = wrap_angle_360(mid_polar + 90.0);
    let strength = (min_val as f64).abs().min((max_val as f64).abs()) / (16.0 * h_rad as f64 * 40.0);
    let fov_sc = (1.0 - span_err / 20.0).max(0.0);
    let conf = (0.55 + 0.30 * strength.min(1.0) + 0.14 * fov_sc).clamp(0.55, 0.99);
    Some((heading_cw, conf))
}

/// Joint 90-degree-paired achromatic white-veil cone detector.
///
/// Genshin's minimap camera cone is rendered as a radial-gradient white veil whose
/// effective alpha fades smoothly from ~0.18-0.28 near the inner radius to ~0.06-0.12
/// toward the rim (and shifts further under HDR tonemapping, dark caves, or snow biomes).
/// Instead of matching a single fixed `alpha = 0.25` LUT with independent `argmax` on the
/// enter/exit edges (which breaks whenever the gradient fades or a bright cliff/building
/// beats one edge), this solves per-pixel implied white-veil alpha `a_c = (C_in - C_out) /
/// max(255 - C_out, 28)` across B/G/R, gates on achromatic consistency (`a_max - a_min <= 0.055`),
/// and jointly maximizes the 90-degree-paired geometric mean `sqrt(enter[i - hf] * exit[i + hf])`.
fn extract_cone_white_veil_joint(
    ann: ArrayView3<'_, u8>,
    _v_prof: &[f32; POLAR_BINS],
    _s_prof: &[f32; POLAR_BINS],
    prior_heading: Option<f64>,
) -> Option<(f64, f64)> {
    let h_rad = ann.dim().1;
    if h_rad < 4 {
        return None;
    }
    let ann_cow = ann.as_standard_layout();
    let ann_s = ann_cow.as_slice()?;

    // Real Genshin minimap cones peak in r = 26..74 (20%..58% R) and fade toward 0 past ~60% R
    // on snow maps, where outer rim snow bleed and pulsing edge quest diamonds live.
    // Focus radial weights strongly on the active inner 65% of the annulus and taper smoothly.
    let mut rad_weights = vec![0.0f32; h_rad];
    let mut w_sum = 0.0f32;
    let denom = (h_rad - 1).max(1) as f32;
    for (x, dst) in rad_weights.iter_mut().enumerate() {
        let t = x as f32 / denom;
        let w = if t <= 0.65 {
            1.60f32 - 1.05f32 * (t / 0.65)
        } else {
            0.55f32 - 0.43f32 * ((t - 0.65) / 0.35)
        };
        *dst = w;
        w_sum += w;
    }
    let inv_w_sum = 1.0f32 / w_sum.max(1e-6);

    let mut m_row = vec![0u8; h_rad];
    let mut e_row = vec![0u8; h_rad];
    let mut profs = [[0.0f32; POLAR_BINS]; 2];

    for (pass_idx, shift) in [CONE_ROLL_SHIFT as isize, -(CONE_ROLL_SHIFT as isize)]
        .into_iter()
        .enumerate()
    {
        let mut row_scores = [0.0f32; POLAR_BINS];
        for (y, dst) in row_scores.iter_mut().enumerate() {
            let sy = (y as isize + shift).rem_euclid(POLAR_BINS as isize) as usize;
            let row_off = y * h_rad;
            let srow_off = sy * h_rad;
            for (x, m_dst) in m_row.iter_mut().enumerate().take(h_rad) {
                let p = (row_off + x) * 3;
                let sp = (srow_off + x) * 3;
                let ob = ann_s[p];
                let og = ann_s[p + 1];
                let or = ann_s[p + 2];
                let db = ann_s[sp] as i16 - ob as i16;
                let dg = ann_s[sp + 1] as i16 - og as i16;
                let dr = ann_s[sp + 2] as i16 - or as i16;
                if db < 3 || dg < 3 || dr < 3 {
                    *m_dst = 0;
                    continue;
                }
                let ab = db as f32 / (255 - ob as i32).max(28) as f32;
                let ag = dg as f32 / (255 - og as i32).max(28) as f32;
                let ar = dr as f32 / (255 - or as i32).max(28) as f32;
                let a_min = ab.min(ag).min(ar);
                let a_max = ab.max(ag).max(ar);
                let a_mean = (ab + ag + ar) * (1.0f32 / 3.0f32);
                *m_dst = if a_min >= 0.045
                    && (0.075..=0.42).contains(&a_mean)
                    && (a_max - a_min) <= 0.055
                {
                    255
                } else {
                    0
                };
            }
            // 1D MorphOp::Open with Rect(2, 1) along radial axis removes isolated 1-px noise.
            e_row[0] = m_row[0];
            for x in 1..h_rad {
                e_row[x] = m_row[x - 1] & m_row[x];
            }
            let mut w_acc = if e_row[0] != 0 { rad_weights[0] } else { 0.0 };
            for x in 1..h_rad {
                if (e_row[x - 1] | e_row[x]) != 0 {
                    w_acc += rad_weights[x];
                }
            }
            *dst = w_acc * inv_w_sum;
        }
        profs[pass_idx] = circular_boxcar_5(&row_scores);
    }

    let enter = &profs[0];
    let exit = &profs[1];
    let mut best_pair = [0.0f32; POLAR_BINS];
    for hf in [60usize, 63, 66, 69, 72] {
        for i in 0..POLAR_BINS {
            let e_in = enter[(i + POLAR_BINS - hf) % POLAR_BINS];
            let e_out = exit[(i + hf) % POLAR_BINS];
            let sc = (e_in * e_out).sqrt();
            if sc > best_pair[i] {
                best_pair[i] = sc;
            }
        }
    }

    // Without a fresh prior, inspect only the strongest global peak (preserving zero extra
    // false-positive risk on negative backgrounds). With a trusted prior during an active sweep,
    // inspect top-3 separated peaks so static bright terrain edges cannot hijack or stall.
    let max_cands = if prior_heading.is_some() { 3 } else { 1 };
    let mut raw_peaks: Vec<(usize, f64)> = Vec::with_capacity(max_cands);
    for _ in 0..max_cands {
        let mut b_idx = None;
        let mut b_val = f32::NEG_INFINITY;
        for (i, &v) in best_pair.iter().enumerate() {
            if raw_peaks
                .iter()
                .all(|&(chosen, _)| circular_bin_dist(i, chosen) > 28)
                && v > b_val
            {
                b_val = v;
                b_idx = Some(i);
            }
        }
        if let Some(idx) = b_idx
            && (b_val as f64) >= 0.065
        {
            raw_peaks.push((idx, b_val as f64));
        } else {
            break;
        }
    }
    let &(top_idx, top_raw_pk) = raw_peaks.first()?;

    let mut bg_g_sum = 0.0f64;
    let mut bg_g_cnt = 0usize;
    for (i, &v) in best_pair.iter().enumerate() {
        if circular_bin_dist(i, top_idx) > 28 {
            bg_g_sum += v as f64;
            bg_g_cnt += 1;
        }
    }
    if bg_g_cnt == 0 {
        return None;
    }
    let bg_mean_global = bg_g_sum / bg_g_cnt as f64;

    // Precompute per-row brightness & desaturation on the active inner cone band (r = 26..76)
    // once so candidate evaluation and local edge checks reuse it cleanly.
    let inner_rad = ((h_rad * 2) / 3).max(4).min(h_rad);
    let inv_v = 1.0 / (255.0 * inner_rad as f64);
    let inv_r = 1.0 / inner_rad as f64;
    let mut row_v = [0.0f64; POLAR_BINS];
    let mut row_s = [0.0f64; POLAR_BINS];
    for i in 0..POLAR_BINS {
        let row_off = i * h_rad;
        let mut rv = 0u64;
        let mut rs = 0.0f64;
        for x in 0..inner_rad {
            let p = (row_off + x) * 3;
            let b = ann_s[p];
            let g = ann_s[p + 1];
            let r = ann_s[p + 2];
            let mx = b.max(g).max(r);
            let mn = b.min(g).min(r);
            rv += mx as u64;
            if mx > 0 {
                rs += (mx - mn) as f64 / mx as f64;
            }
        }
        row_v[i] = rv as f64 * inv_v;
        row_s[i] = rs * inv_r;
    }

    #[derive(Clone, Copy)]
    struct VeilCand {
        idx: usize,
        pk: f64,
        snr: f64,
        heading_cw: f64,
    }

    let mut valid: Vec<VeilCand> = Vec::with_capacity(raw_peaks.len());
    for &(cand_idx, pk_f) in &raw_peaks {
        let mut bg_sum = 0.0f64;
        let mut bg_cnt = 0usize;
        for (i, &v) in best_pair.iter().enumerate() {
            if circular_bin_dist(i, cand_idx) > 28 {
                bg_sum += v as f64;
                bg_cnt += 1;
            }
        }
        if bg_cnt == 0 {
            continue;
        }
        let bg_m = bg_sum / bg_cnt as f64;
        let snr = pk_f / (bg_m.min(bg_mean_global) + 1e-4);
        if snr < 3.2 {
            continue;
        }

        let mut in_v_sum = 0.0f64;
        let mut in_s_sum = 0.0f64;
        let mut in_cnt = 0usize;
        let mut out_v_sum = 0.0f64;
        let mut out_s_sum = 0.0f64;
        let mut out_cnt = 0usize;
        let mut loc_in_v_sum = 0.0f64;
        let mut loc_in_s_sum = 0.0f64;
        let mut loc_in_cnt = 0usize;
        let mut loc_out_v_sum = 0.0f64;
        let mut loc_out_s_sum = 0.0f64;
        let mut loc_out_cnt = 0usize;

        for i in 0..POLAR_BINS {
            let cd = circular_bin_dist(i, cand_idx);
            if cd <= 56 {
                in_v_sum += row_v[i];
                in_s_sum += row_s[i];
                in_cnt += 1;
            } else if (72..=112).contains(&cd) {
                out_v_sum += row_v[i];
                out_s_sum += row_s[i];
                out_cnt += 1;
            }
            if (48..=62).contains(&cd) {
                loc_in_v_sum += row_v[i];
                loc_in_s_sum += row_s[i];
                loc_in_cnt += 1;
            } else if (70..=84).contains(&cd) {
                loc_out_v_sum += row_v[i];
                loc_out_s_sum += row_s[i];
                loc_out_cnt += 1;
            }
        }
        if in_cnt == 0 || out_cnt == 0 || loc_in_cnt == 0 || loc_out_cnt == 0 {
            continue;
        }

        let in_v = in_v_sum / in_cnt as f64;
        let out_v = out_v_sum / out_cnt as f64;
        let in_s = in_s_sum / in_cnt as f64;
        let out_s = out_s_sum / out_cnt as f64;
        let loc_in_v = loc_in_v_sum / loc_in_cnt as f64;
        let loc_in_s = loc_in_s_sum / loc_in_cnt as f64;
        let loc_out_v = loc_out_v_sum / loc_out_cnt as f64;
        let loc_out_s = loc_out_s_sum / loc_out_cnt as f64;

        // Require either global brightness/desaturation contrast, local cone-edge brightening
        // without saturation jump (dark terrain corridor flanked by brighter rock), or
        // strong high-SNR 90-deg edge pairing. Still blocks dark desaturated structures.
        let wedge_ok = in_v > out_v + 0.005
            || (out_s > in_s + 0.015 && in_v >= out_v - 0.03)
            || (out_s > in_s + 0.08 && in_v >= out_v - 0.07)
            || (loc_in_v > loc_out_v + 0.015
                && loc_out_s >= loc_in_s - 0.01
                && in_v >= out_v - 0.08)
            || (snr >= 12.0 && pk_f >= 0.22 && in_v >= out_v - 0.08)
            || (snr >= 15.0 && pk_f >= 0.22);
        if !wedge_ok {
            continue;
        }

        let c_sub = circular_subpixel_peak(&best_pair, cand_idx, false);
        let heading_cw = wrap_angle_360((c_sub / POLAR_BINS as f64) * 360.0 + 90.0);
        valid.push(VeilCand {
            idx: cand_idx,
            pk: pk_f,
            snr,
            heading_cw,
        });
    }

    if valid.is_empty() {
        return None;
    }

    let (chosen, prior_matched) = if let Some(prior) = prior_heading {
        let d_top = shortest_angle_delta(prior, valid[0].heading_cw).abs();
        if let Some(&p_cand) = valid
            .iter()
            .filter(|c| {
                let d_prior = shortest_angle_delta(prior, c.heading_cw).abs();
                d_prior <= 32.0
                    && c.pk >= 0.075
                    && c.pk >= 0.50 * top_raw_pk
                    && (d_top > 60.0 || c.pk >= 0.75 * valid[0].pk)
            })
            .max_by(|a, b| a.pk.total_cmp(&b.pk))
        {
            (p_cand, true)
        } else {
            (valid[0], false)
        }
    } else {
        (valid[0], false)
    };

    let mut other_pk = 0.0f64;
    for (i, &v) in best_pair.iter().enumerate() {
        if circular_bin_dist(i, chosen.idx) > 28 {
            if prior_matched
                && let Some(prior) = prior_heading
            {
                let h = wrap_angle_360((i as f64 / POLAR_BINS as f64) * 360.0 + 90.0);
                if shortest_angle_delta(prior, h).abs() > 55.0 {
                    continue;
                }
            }
            let vf = v as f64;
            if vf > other_pk {
                other_pk = vf;
            }
        }
    }

    if !prior_matched && chosen.pk < 0.22 && other_pk > 0.0 && chosen.pk < 1.25 * other_pk {
        // Reject un-priored ambiguous peaks competing with a bright static terrain/snow edge
        // (>20 deg away) so snow maps never flip-flop between two near-tied terrain edges.
        return None;
    }

    let fill_sc = (chosen.pk / 0.35).min(1.0);
    let snr_sc = ((chosen.snr - 3.0) / 14.0).clamp(0.0, 1.0);
    let dom_ratio = if other_pk > 1e-4 {
        (chosen.pk / other_pk).clamp(1.0, 3.0)
    } else {
        3.0
    };
    let dom_sc = ((dom_ratio - 1.0) / 1.5).clamp(0.0, 1.0);
    let conf = (0.48 + 0.24 * fill_sc + 0.16 * snr_sc + 0.08 * dom_sc).clamp(0.48, 0.96);
    Some((chosen.heading_cw, conf))
}

fn extract_cone_bgr_blend_simd(
    ann: ArrayView3<'_, u8>,
    v_prof: &[f32; POLAR_BINS],
    s_prof: &[f32; POLAR_BINS],
    prior_heading: Option<f64>,
) -> Option<(f64, f64)> {
    // 1. Razor-sharp SDR 25% white-veil LUT fast path (tight span <= 10 deg, fill >= 0.10,
    //    SNR >= 6.0) so asymmetric snow/cliff edges never hijack `white_veil_joint` mid-spin.
    //    If a fresh prior contradicts a distant LUT jump (> 60 deg), prefer a joint-veil match
    //    within 32 deg of the prior first.
    if let Some((deg, conf)) =
        extract_cone_bgr_blend_with_lut(ann, v_prof, lut_pred_u8(), 8, 10.0, 0.10, 6.0)
    {
        if let Some(prior) = prior_heading
            && shortest_angle_delta(prior, deg).abs() > 60.0
            && let Some((w_deg, w_conf)) =
                extract_cone_white_veil_joint(ann, v_prof, s_prof, Some(prior))
            && shortest_angle_delta(prior, w_deg).abs() <= 32.0
        {
            return Some((w_deg, w_conf));
        }
        return Some((deg, conf));
    }
    // 2. Joint 90-deg-paired achromatic white-veil invariant: handles radial-gradient cones,
    //    bright snow biomes, dark underground caves, and HDR-shifted alpha cones smoothly.
    if let Some(res) = extract_cone_white_veil_joint(ann, v_prof, s_prof, prior_heading) {
        return Some(res);
    }
    // 3. Relaxed SDR 25% LUT fallback (only when joint white-veil did not lock).
    if let Some(res) =
        extract_cone_bgr_blend_with_lut(ann, v_prof, lut_pred_u8(), 8, 20.0, 0.04, 2.2)
    {
        return Some(res);
    }
    // 4. Strict HDR linear-light fallback (WGC RGBA16F -> sRGB): catches pure 25% linear-light
    //    cones on dark terrain whose sRGB implied alpha exceeds 0.42.
    extract_cone_bgr_blend_with_lut(ann, v_prof, lut_pred_linear_u8(), 9, 14.0, 0.14, 3.8)
}

fn extract_cone_bgr_blend_with_lut(
    ann: ArrayView3<'_, u8>,
    v_prof: &[f32; POLAR_BINS],
    lut: &[u8; 256],
    chan_tol: i16,
    span_tol_deg: f64,
    min_fill: f64,
    min_snr: f64,
) -> Option<(f64, f64)> {
    let h_rad = ann.dim().1;
    let ann_cow = ann.as_standard_layout();
    let ann_s = ann_cow.as_slice()?;
    let mut ann_gray = vec![0u8; POLAR_BINS * h_rad];
    for (i, dst) in ann_gray.iter_mut().enumerate() {
        let p = i * 3;
        *dst = bgr_px_to_gray(ann_s[p], ann_s[p + 1], ann_s[p + 2]);
    }
    let mut m_row = vec![0u8; h_rad];
    let mut e_row = vec![0u8; h_rad];

    let mut angles = [0.0f64; 2];
    let mut pvals = [0.0f64; 2];
    let mut snrs = [0.0f64; 2];

    // Loop order matches Python `for off in (2 * _CONE_ROLL_SHIFT, 0)`:
    // iter 0 -> row (y + 5) % 512 (roll(-5)); iter 1 -> row (y - 5) % 512 (roll(+5)).
    for (pass_idx, shift) in [CONE_ROLL_SHIFT as isize, -(CONE_ROLL_SHIFT as isize)]
        .into_iter()
        .enumerate()
    {
        let mut row_sums = [0.0f32; POLAR_BINS];
        for (y, sum_dst) in row_sums.iter_mut().enumerate() {
            let sy = (y as isize + shift).rem_euclid(POLAR_BINS as isize) as usize;
            let row_off = y * h_rad;
            let srow_off = sy * h_rad;
            for x in 0..h_rad {
                let p = (row_off + x) * 3;
                let sp = (srow_off + x) * 3;
                let db = (lut[ann_s[p] as usize] as i16 - ann_s[sp] as i16).abs();
                let dg = (lut[ann_s[p + 1] as usize] as i16 - ann_s[sp + 1] as i16).abs();
                let dr = (lut[ann_s[p + 2] as usize] as i16 - ann_s[sp + 2] as i16).abs();
                m_row[x] = if db <= chan_tol
                    && dg <= chan_tol
                    && dr <= chan_tol
                    && ann_gray[srow_off + x] > ann_gray[row_off + x].saturating_add(3)
                {
                    255
                } else {
                    0
                };
            }
            // Bit-exact MorphOp::Open with Rect(2, 1) (anchor (1, 0): offsets (-1, 0), (0, 0))
            e_row[0] = m_row[0];
            for x in 1..h_rad {
                e_row[x] = m_row[x - 1] & m_row[x];
            }
            let mut sum_u32 = e_row[0] as u32;
            for x in 1..h_rad {
                sum_u32 += (e_row[x - 1] | e_row[x]) as u32;
            }
            *sum_dst = sum_u32 as f32;
        }
        let cs = circular_boxcar_5(&row_sums);
        let mut pk_i = 0usize;
        let mut pk = f32::NEG_INFINITY;
        let mut total = 0.0f64;
        for (i, &v) in cs.iter().enumerate() {
            total += v as f64;
            if v > pk {
                pk = v;
                pk_i = i;
            }
        }
        let pk_f = pk as f64;
        let mn = total / POLAR_BINS as f64;
        if pk_f < min_fill * (h_rad as f64) * 255.0 || pk_f < min_snr * (mn + 1e-3) {
            return None;
        }
        angles[pass_idx] = (circular_subpixel_peak(&cs, pk_i, false) / POLAR_BINS as f64) * 360.0;
        pvals[pass_idx] = pk_f / (h_rad as f64 * 255.0);
        snrs[pass_idx] = pk_f / (mn + 1e-3);
    }

    let span = (angles[1] - angles[0]).rem_euclid(360.0);
    if (span - 90.0).abs() > span_tol_deg {
        return None;
    }

    let mid_polar = angles[0] + span * 0.5;
    let mid_bin = ((mid_polar.rem_euclid(360.0) / 360.0 * POLAR_BINS as f64).round_ties_even()
        as usize)
        % POLAR_BINS;
    if !wedge_brightness_ok(v_prof, mid_bin) {
        return None;
    }

    let heading_cw = wrap_angle_360(mid_polar + 90.0);
    let fill_sc = (pvals[0].min(pvals[1]) / 0.35).min(1.0);
    let snr_sc = ((snrs[0].min(snrs[1]) - 2.0) / 10.0).min(1.0);
    let fov_sc = (1.0 - (span - 90.0).abs() / 25.0).max(0.0);
    let conf = (0.45 + 0.25 * fill_sc + 0.15 * snr_sc + 0.10 * fov_sc).clamp(0.45, 0.95);
    Some((heading_cw, conf))
}

fn extract_cone_hsv_adaptive_polar(
    ann: ArrayView3<'_, u8>,
    v_prof: &[f32; POLAR_BINS],
) -> Option<(f64, f64)> {
    let hsv = bgr_to_hsv(ann).ok()?;
    let h_rad = ann.dim().1;
    let mut contrast = Array2::<f32>::zeros((POLAR_BINS, h_rad));
    let sat_scale = 0.25f32 / 255.0f32;
    let val_scale = 1.0f32 / 255.0f32;
    for y in 0..POLAR_BINS {
        for x in 0..h_rad {
            let v = hsv[[y, x, 2]] as f32 * val_scale;
            let s = hsv[[y, x, 1]] as f32 * sat_scale;
            contrast[[y, x]] = v - s;
        }
    }

    let padded = copy_make_border_f32(contrast.view(), 2, 2, 0, 0, BorderMode::Wrap).ok()?;
    let gy_full = scharr_dy_f32(padded.view()).ok()?;
    let mut row_means = [0.0f32; POLAR_BINS];
    let inv_16 = 1.0f32 / 16.0f32;
    let inv_rad = 1.0f32 / h_rad as f32;
    for (y, dst) in row_means.iter_mut().enumerate() {
        let mut s = 0.0f32;
        for x in 0..h_rad {
            s += (gy_full[[y + 2, x]] * inv_16).clamp(-0.35, 0.35);
        }
        *dst = s * inv_rad;
    }
    let g = circular_boxcar_5(&row_means);

    let mut best_score = [0.0f32; POLAR_BINS];
    for half_fov in [60usize, 64, 68] {
        for i in 0..POLAR_BINS {
            let enter = g[(i + POLAR_BINS - half_fov) % POLAR_BINS].max(0.0);
            let exit = (-g[(i + half_fov) % POLAR_BINS]).max(0.0);
            let sc = (enter * exit).sqrt();
            if sc > best_score[i] {
                best_score[i] = sc;
            }
        }
    }

    let mut best_idx = 0usize;
    let mut peak = f32::NEG_INFINITY;
    for (i, &v) in best_score.iter().enumerate() {
        if v > peak {
            peak = v;
            best_idx = i;
        }
    }
    let peak_f = peak as f64;
    if peak_f < 0.015 {
        return None;
    }

    let mut bg: Vec<f64> = Vec::with_capacity(POLAR_BINS);
    for (i, &v) in best_score.iter().enumerate() {
        if circular_bin_dist(i, best_idx) > 24 {
            bg.push(v as f64);
        }
    }
    if bg.is_empty() {
        return None;
    }
    let n = bg.len();
    let mean = bg.iter().sum::<f64>() / n as f64;
    let var = bg.iter().map(|&x| (x - mean) * (x - mean)).sum::<f64>() / n as f64;
    let bg_std = var.sqrt();

    bg.sort_by(|a, b| a.total_cmp(b));
    let bg_med = if n.is_multiple_of(2) {
        0.5 * (bg[n / 2 - 1] + bg[n / 2])
    } else {
        bg[n / 2]
    };
    let pos_98 = 0.98 * (n - 1) as f64;
    let lo = pos_98.floor() as usize;
    let hi = pos_98.ceil() as usize;
    let frac = pos_98 - lo as f64;
    let bg_p98 = bg[lo] * (1.0 - frac) + bg[hi] * frac;

    if peak_f <= bg_med + 3.5 * bg_std.max(1e-4) || peak_f <= 1.2 * bg_p98 {
        return None;
    }
    if !wedge_brightness_ok(v_prof, best_idx) {
        return None;
    }

    let c_sub = circular_subpixel_peak(&best_score, best_idx, false);
    let heading_cw = wrap_angle_360((c_sub / POLAR_BINS as f64) * 360.0 + 90.0);
    let prom = (peak_f - bg_med) / (peak_f + 0.05);
    let conf = (0.35 + 0.47 * prom).clamp(0.35, 0.82);
    Some((heading_cw, conf))
}

pub fn extract_camera_view_cone(minimap_img: ArrayView3<'_, u8>) -> ConeDetection {
    extract_camera_view_cone_with_prior(minimap_img, None)
}

pub fn extract_camera_view_cone_with_prior(
    minimap_img: ArrayView3<'_, u8>,
    prior_heading: Option<f64>,
) -> ConeDetection {
    let (h, w, c) = minimap_img.dim();
    if (c != 3 && c != 4) || h.min(w) < 24 {
        return ConeDetection::default();
    }

    // Tier 1: BGRA alpha-channel fast path
    if has_non_opaque_alpha(minimap_img) {
        let alpha = minimap_img.slice(s![.., .., 3]);
        if let Some((deg, conf)) = extract_cone_bgra_alpha_polar(alpha) {
            return ConeDetection {
                heading_deg: Some(deg),
                confidence: conf,
                method: ConeMethod::BgraAlpha,
            };
        }
    }

    let bgr = if c == 4 {
        minimap_img.slice(s![.., .., 0..3])
    } else {
        minimap_img
    };
    let center = (w as f64 * 0.5, h as f64 * 0.5);
    let max_rad = (h.min(w) as f64) * 0.5;
    let Ok(ann) = warp_polar_annulus_u8_hwc(
        bgr,
        RAD_BINS,
        POLAR_BINS,
        (RAD_R0, RAD_R1),
        center,
        max_rad,
        Interpolation::Linear,
    ) else {
        return ConeDetection::default();
    };

    // Exact V (= max(B, G, R) / 255) and S (= (max - min) / max) row profiles in one pass.
    let h_rad = ann.dim().1;
    let mut v_prof = [0.0f32; POLAR_BINS];
    let mut s_prof = [0.0f32; POLAR_BINS];
    let inv_v = 1.0 / (255.0 * h_rad as f64);
    let inv_h = 1.0 / h_rad as f64;
    for y in 0..POLAR_BINS {
        let mut sum_v = 0u64;
        let mut sum_s = 0.0f64;
        for x in 0..h_rad {
            let b = ann[[y, x, 0]];
            let g = ann[[y, x, 1]];
            let r = ann[[y, x, 2]];
            let mx = b.max(g).max(r);
            let mn = b.min(g).min(r);
            sum_v += mx as u64;
            if mx > 0 {
                sum_s += (mx - mn) as f64 / mx as f64;
            }
        }
        v_prof[y] = (sum_v as f64 * inv_v) as f32;
        s_prof[y] = (sum_s * inv_h) as f32;
    }

    if let Some((deg, conf)) =
        extract_cone_bgr_blend_simd(ann.view(), &v_prof, &s_prof, prior_heading)
    {
        return ConeDetection {
            heading_deg: Some(deg),
            confidence: conf,
            method: ConeMethod::BgrBlend,
        };
    }

    if let Some((deg, conf)) = extract_cone_hsv_adaptive_polar(ann.view(), &v_prof) {
        return ConeDetection {
            heading_deg: Some(deg),
            confidence: conf,
            method: ConeMethod::HsvAdaptive,
        };
    }

    ConeDetection::default()
}

fn frame_view_to_bgr_or_bgra(frame: FrameView<'_>) -> Option<Array3<u8>> {
    match frame.format {
        PixelFormat::Gray8 => None,
        PixelFormat::Bgr8 | PixelFormat::Bgra8 => frame_view_to_array3(frame).ok(),
        PixelFormat::Rgba8 => {
            let rgba = frame_view_to_array3(frame).ok()?;
            swap_rb(rgba.view()).ok()
        }
    }
}

pub fn extract_camera_view_cone_frame(frame: FrameView<'_>) -> ConeDetection {
    extract_camera_view_cone_frame_with_prior(frame, None)
}

pub fn extract_camera_view_cone_frame_with_prior(
    frame: FrameView<'_>,
    prior_heading: Option<f64>,
) -> ConeDetection {
    let Some(arr) = frame_view_to_bgr_or_bgra(frame) else {
        return ConeDetection::default();
    };
    extract_camera_view_cone_with_prior(arr.view(), prior_heading)
}

const ARROW_POLAR_BINS: usize = 128;
const ARROW_RAD_FULL: usize = 24;
const ARROW_RAD_IN: usize = 9;
const ARROW_RAD_OUT: usize = 24;
const ARROW_RAD_LEN: usize = ARROW_RAD_OUT - ARROW_RAD_IN;

type ArrowPolarTemplate = [[f32; ARROW_RAD_LEN]; ARROW_POLAR_BINS];

struct ArrowPolarTemplates {
    genshin_centroid: ArrowPolarTemplate,
    dart_centroid: ArrowPolarTemplate,
    genshin_pivot: ArrowPolarTemplate,
    dart_pivot: ArrowPolarTemplate,
}

fn normalize_arrow_polar(pol: ArrayView2<'_, u8>, roll_rows: usize) -> Option<ArrowPolarTemplate> {
    let mut out = [[0.0f32; ARROW_RAD_LEN]; ARROW_POLAR_BINS];
    let mut sum = 0.0f64;
    let total = (ARROW_POLAR_BINS * ARROW_RAD_LEN) as f64;
    for (dst_y, dst_row) in out.iter_mut().enumerate() {
        let src_y = (dst_y + roll_rows) % ARROW_POLAR_BINS;
        for (x, slot) in dst_row.iter_mut().enumerate() {
            let v = pol[[src_y, x]] as f32 * (1.0f32 / 255.0f32);
            *slot = v;
            sum += v as f64;
        }
    }
    let mean = (sum / total) as f32;
    let mut norm_sq = 0.0f64;
    for row in &mut out {
        for v in row.iter_mut() {
            *v -= mean;
            norm_sq += (*v as f64) * (*v as f64);
        }
    }
    let norm = norm_sq.sqrt();
    if norm < 1e-4 {
        return None;
    }
    let inv_norm = (1.0 / norm) as f32;
    for row in &mut out {
        for v in row.iter_mut() {
            *v *= inv_norm;
        }
    }
    Some(out)
}

fn build_arrow_polar_template(
    polys: &[&[(f64, f64)]],
    r_max: f64,
    recenter: bool,
) -> ArrowPolarTemplate {
    const SZ: usize = 128;
    const C: f64 = 64.0;
    let sc = 48.0 / r_max;
    let mut img = Array2::<u8>::zeros((SZ, SZ));
    for poly in polys {
        let pts: Vec<Point2i> = poly
            .iter()
            .map(|&(x, y)| Point2i {
                x: (C + x * sc).round() as i32,
                y: (C + y * sc).round() as i32,
            })
            .collect();
        draw_filled_convex_poly_u8(&mut img, &pts, 255);
    }
    let center = if recenter {
        let mut sx = 0.0f64;
        let mut sy = 0.0f64;
        let mut cnt = 0usize;
        for y in 0..SZ {
            for x in 0..SZ {
                if img[[y, x]] != 0 {
                    sx += x as f64;
                    sy += y as f64;
                    cnt += 1;
                }
            }
        }
        if cnt > 0 {
            (sx / cnt as f64, sy / cnt as f64)
        } else {
            (C, C)
        }
    } else {
        (C, C)
    };
    let pol = warp_polar_annulus_u8(
        img.view(),
        ARROW_RAD_FULL,
        ARROW_POLAR_BINS,
        (ARROW_RAD_IN, ARROW_RAD_OUT),
        center,
        56.0,
        Interpolation::Linear,
    )
    .expect("arrow polar template warp");
    // Template polygon nose points North (-Y, polar 270 deg = bin 96); roll so nose sits at bin 0.
    normalize_arrow_polar(pol.view(), (3 * ARROW_POLAR_BINS) / 4).expect("non-empty arrow template")
}

fn arrow_polar_templates() -> &'static ArrowPolarTemplates {
    static TEMPLATES: OnceLock<ArrowPolarTemplates> = OnceLock::new();
    TEMPLATES.get_or_init(|| {
        // Real Genshin minimap cyan arrow geometry (calibrated from 104 clean walking frames):
        // sharp nose at 0 deg, symmetric wings at +-122 deg, twin notches + protruding tail tab at 180 deg.
        let genshin_polys: &[&[(f64, f64)]] = &[
            &[(0.0, -1.00), (-0.84, 0.51), (0.0, 0.51)],
            &[(0.0, -1.00), (0.0, 0.51), (0.84, 0.51)],
            &[(-0.31, 0.49), (0.0, 0.70), (0.31, 0.49)],
        ];
        // Classic V-notch dart geometry (synthetic test fixture / legacy shape).
        let dart_polys: &[&[(f64, f64)]] = &[
            &[(0.0, -13.0), (-8.5, 9.0), (0.0, 4.0)],
            &[(0.0, -13.0), (0.0, 4.0), (8.5, 9.0)],
        ];
        ArrowPolarTemplates {
            genshin_centroid: build_arrow_polar_template(genshin_polys, 1.0, true),
            dart_centroid: build_arrow_polar_template(dart_polys, 13.0, true),
            genshin_pivot: build_arrow_polar_template(genshin_polys, 1.0, false),
            dart_pivot: build_arrow_polar_template(dart_polys, 13.0, false),
        }
    })
}

fn nonzero_bbox_padded(arr: ArrayView2<'_, u8>, pad: usize) -> Option<(usize, usize, usize, usize)> {
    let (h, w) = arr.dim();
    let arr_contig;
    let s = if let Some(sl) = arr.as_slice() {
        sl
    } else {
        arr_contig = arr.to_owned();
        arr_contig.as_slice().unwrap()
    };
    let mut min_x = w;
    let mut max_x = 0usize;
    let mut min_y = h;
    let mut max_y = 0usize;
    for y in 0..h {
        let row = &s[y * w..(y + 1) * w];
        for (x, &v) in row.iter().enumerate() {
            if v != 0 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if min_x > max_x || min_y > max_y {
        return None;
    }
    Some((
        min_x.saturating_sub(pad),
        min_y.saturating_sub(pad),
        w.min(max_x + 1 + pad),
        h.min(max_y + 1 + pad),
    ))
}

/// Isolates the central player-arrow component(s) while excluding nearby cyan/blue map pins
/// (waypoints, snowflakes, co-op/quest icons) near the outer edges of the `0.4..0.6` crop.
fn isolate_arrow_mask(mask2: ArrayView2<'_, u8>) -> Option<(Array2<u8>, f64, f64, f64)> {
    let (nx0, ny0, nx1, ny1) = nonzero_bbox_padded(mask2, 0)?;
    let roi = mask2.slice(s![ny0..ny1, nx0..nx1]);
    let cc = connected_components_with_stats_8(roi).ok()?;
    if cc.num_labels <= 1 {
        return None;
    }
    let (mh, mw) = mask2.dim();
    let cx0 = mw as f64 * 0.5;
    let cy0 = mh as f64 * 0.5;
    let min_dim = mh.min(mw) as f64;
    let max_center_dist = 0.28 * min_dim;

    let ox = nx0 as f64;
    let oy = ny0 as f64;
    let mut best_label: Option<usize> = None;
    let mut best_area = 0i32;
    for lbl in 1..cc.num_labels {
        let area = cc.stats[lbl].area;
        let dist = ((cc.centroids[lbl][0] + ox) - cx0).hypot((cc.centroids[lbl][1] + oy) - cy0);
        if dist <= max_center_dist && area > best_area {
            best_area = area;
            best_label = Some(lbl);
        }
    }
    let best_lbl = best_label?;
    if best_area < 16 {
        return None;
    }

    let bcx = cc.centroids[best_lbl][0] + ox;
    let bcy = cc.centroids[best_lbl][1] + oy;
    let min_keep_area = 8.max((0.10 * best_area as f64) as i32);
    let mut keep = vec![false; cc.num_labels];
    for (lbl, slot) in keep.iter_mut().enumerate().skip(1) {
        let area = cc.stats[lbl].area;
        let lx = cc.centroids[lbl][0] + ox;
        let ly = cc.centroids[lbl][1] + oy;
        let d_center = (lx - cx0).hypot(ly - cy0);
        let d_best = (lx - bcx).hypot(ly - bcy);
        if lbl == best_lbl
            || (d_center <= 0.34 * min_dim && d_best <= 0.30 * min_dim && area >= min_keep_area)
        {
            *slot = true;
        }
    }

    let mut cleaned = Array2::<u8>::zeros((mh, mw));
    let rw = nx1 - nx0;
    let lbls = cc.labels.as_slice().expect("contiguous labels");
    let cleaned_s = cleaned.as_slice_mut().expect("contiguous cleaned");
    let mut sx = 0.0f64;
    let mut sy = 0.0f64;
    let mut cnt = 0usize;
    for ry in 0..(ny1 - ny0) {
        let gy = ny0 + ry;
        let dst_row = &mut cleaned_s[gy * mw + nx0..gy * mw + nx1];
        let src_row = &lbls[ry * rw..(ry + 1) * rw];
        for (rx, (dst, &lbl)) in dst_row.iter_mut().zip(src_row.iter()).enumerate() {
            if lbl > 0 && keep[lbl as usize] {
                *dst = 255;
                sx += (nx0 + rx) as f64;
                sy += gy as f64;
                cnt += 1;
            }
        }
    }
    if cnt < 16 {
        return None;
    }
    let mcx = sx / cnt as f64;
    let mcy = sy / cnt as f64;
    let mut r_obs = 0.0f64;
    for gy in ny0..ny1 {
        let row = &cleaned_s[gy * mw + nx0..gy * mw + nx1];
        for (rx, &v) in row.iter().enumerate() {
            if v != 0 {
                let d = ((nx0 + rx) as f64 - mcx).hypot(gy as f64 - mcy);
                if d > r_obs {
                    r_obs = d;
                }
            }
        }
    }
    Some((cleaned, mcx, mcy, r_obs))
}

#[inline]
fn circular_arrow_bin_dist(a: usize, b: usize) -> usize {
    let d1 = (a + ARROW_POLAR_BINS - b) % ARROW_POLAR_BINS;
    let d2 = (b + ARROW_POLAR_BINS - a) % ARROW_POLAR_BINS;
    d1.min(d2)
}

fn hist_quantile_u8(hist: &[u32; 256], n: usize, q: f64) -> u8 {
    let target = (q.clamp(0.0, 1.0) * n.saturating_sub(1) as f64).round() as usize;
    let mut cum = 0usize;
    for (val, &cnt) in hist.iter().enumerate() {
        cum += cnt as usize;
        if cum > target {
            return val as u8;
        }
    }
    255
}

/// Breaks Genshin's near-equilateral ±120° arrow symmetry by extracting the bright cyan-white
/// tail jewel sitting at `180°` opposite the nose on real captured arrows. Cleanly returns `None`
/// on flat or symmetric synthetic arrows (`G_p95 - G_p50 < 12` or `jdist < 0.18 * r_use`).
fn extract_tail_jewel_prior(
    up2: ArrayView3<'_, u8>,
    clean2: ArrayView2<'_, u8>,
    mcx: f64,
    mcy: f64,
    r_use: f64,
) -> Option<(f64, f64)> {
    let (uh, uw) = clean2.dim();
    let mut hist = [0u32; 256];
    let mut n = 0usize;
    for y in 0..uh {
        for x in 0..uw {
            if clean2[[y, x]] != 0 {
                hist[up2[[y, x, 1]] as usize] += 1;
                n += 1;
            }
        }
    }
    if n < 64 {
        return None;
    }
    let g_p50 = hist_quantile_u8(&hist, n, 0.50);
    let g_p90 = hist_quantile_u8(&hist, n, 0.90);
    let g_p95 = hist_quantile_u8(&hist, n, 0.95);
    let contrast = g_p95 as f64 - g_p50 as f64;
    if contrast < 12.0 {
        return None;
    }

    let g_thresh = g_p50.saturating_add(12).max(g_p90);
    let r_min = 0.14 * r_use;
    let mut sum_dx = 0.0f64;
    let mut sum_dy = 0.0f64;
    let mut sum_ux = 0.0f64;
    let mut sum_uy = 0.0f64;
    let mut jcnt = 0usize;
    for y in 0..uh {
        for x in 0..uw {
            if clean2[[y, x]] != 0 && up2[[y, x, 1]] >= g_thresh {
                let dx = x as f64 - mcx;
                let dy = y as f64 - mcy;
                let dist = dx.hypot(dy);
                if dist >= r_min {
                    sum_dx += dx;
                    sum_dy += dy;
                    sum_ux += dx / dist;
                    sum_uy += dy / dist;
                    jcnt += 1;
                }
            }
        }
    }
    if jcnt < 12.max(n / 25) {
        return None;
    }
    let jdx_mean = sum_dx / jcnt as f64;
    let jdy_mean = sum_dy / jcnt as f64;
    let jdist = jdx_mean.hypot(jdy_mean);
    if jdist < 0.18 * r_use {
        return None;
    }
    let r_bar = (sum_ux / jcnt as f64).hypot(sum_uy / jcnt as f64);
    if r_bar < 0.60 {
        return None;
    }

    let nose_deg = wrap_angle_360((-jdx_mean).atan2(jdy_mean).to_degrees());
    let strength = (0.35
        + 0.65
            * ((contrast - 10.0) / 15.0).clamp(0.0, 1.0)
            * ((r_bar - 0.55) / 0.35).clamp(0.0, 1.0)
            * ((jdist / r_use - 0.16) / 0.16).clamp(0.0, 1.0))
    .clamp(0.35, 1.0);
    Some((nose_deg, strength))
}

fn score_arrow_polar_template(
    obs: &ArrowPolarTemplate,
    tmpl: &ArrowPolarTemplate,
    conf_scale: f64,
    jewel_prior: Option<(f64, f64)>,
) -> Option<(f64, f64, f64)> {
    let mut corr = [0.0f32; ARROW_POLAR_BINS];
    for (shift, slot) in corr.iter_mut().enumerate() {
        let mut acc = 0.0f32;
        for (t, t_row) in tmpl.iter().enumerate() {
            let o_row = &obs[(t + shift) & (ARROW_POLAR_BINS - 1)];
            for r in 0..ARROW_RAD_LEN {
                acc += o_row[r] * t_row[r];
            }
        }
        *slot = acc;
    }

    let mut scored = [0.0f64; ARROW_POLAR_BINS];
    for (i, (&c, slot)) in corr.iter().zip(scored.iter_mut()).enumerate() {
        let bonus = if let Some((j_deg, j_str)) = jewel_prior {
            let bin_deg = (i as f64 / ARROW_POLAR_BINS as f64) * 360.0 + 90.0;
            0.30 * j_str * shortest_angle_delta(j_deg, bin_deg).to_radians().cos()
        } else {
            0.0
        };
        *slot = c as f64 + bonus;
    }

    let mut best_scored_idx = 0usize;
    let mut best_scored_val = f64::NEG_INFINITY;
    for (i, &v) in scored.iter().enumerate() {
        if v > best_scored_val {
            best_scored_val = v;
            best_scored_idx = i;
        }
    }

    // Refine to raw correlation local maximum within +/-3 bins so jewel prior selects the
    // correct +/-120 lobe without pulling the sub-degree ridge away from exact shape fit.
    let mut best_idx = best_scored_idx;
    let mut best_raw = corr[best_scored_idx];
    for d in -3isize..=3 {
        let idx = (best_scored_idx as isize + d).rem_euclid(ARROW_POLAR_BINS as isize) as usize;
        if corr[idx] > best_raw {
            best_raw = corr[idx];
            best_idx = idx;
        }
    }

    let pk_f = corr[best_idx] as f64;
    let min_raw_corr = if jewel_prior.is_some() { 0.42 } else { 0.45 };
    if pk_f < min_raw_corr {
        return None;
    }

    let mut second_scored = f64::NEG_INFINITY;
    for (i, &v) in scored.iter().enumerate() {
        if circular_arrow_bin_dist(i, best_idx) >= 16 && v > second_scored {
            second_scored = v;
        }
    }
    let margin = scored[best_idx] - second_scored.max(0.0);
    if margin < 0.008 {
        return None;
    }

    let y_prev = corr[(best_idx + ARROW_POLAR_BINS - 1) & (ARROW_POLAR_BINS - 1)] as f64;
    let y_curr = pk_f;
    let y_next = corr[(best_idx + 1) & (ARROW_POLAR_BINS - 1)] as f64;
    let denom = y_prev - 2.0 * y_curr + y_next;
    let off = if denom.abs() > 1e-9 {
        (0.5 * (y_prev - y_next) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let sub_bin = (best_idx as f64 + off).rem_euclid(ARROW_POLAR_BINS as f64);
    let deg = wrap_angle_360((sub_bin / ARROW_POLAR_BINS as f64) * 360.0 + 90.0);

    // Weight both jewel-disambiguated correlation and margin over +-120 deg wing confusion peaks.
    let quality = scored[best_idx] + 1.5 * margin.min(0.25);
    let pk_sc = ((pk_f - 0.50) / 0.45).clamp(0.0, 1.0);
    let margin_sc = (margin / 0.04).clamp(0.0, 1.0);
    let conf = ((0.35 + 0.45 * pk_sc + 0.18 * margin_sc) * conf_scale).clamp(0.35, 0.98);
    Some((quality, deg, conf))
}

fn solve_arrow_from_mask(
    up2: ArrayView3<'_, u8>,
    mask2: ArrayView2<'_, u8>,
    conf_scale: f64,
) -> Option<(f64, f64)> {
    let nz_raw = if let Some(s) = mask2.as_slice() {
        s.iter().filter(|&&v| v != 0).count()
    } else {
        mask2.iter().filter(|&&v| v != 0).count()
    };
    if nz_raw < 16 {
        return None;
    }
    let (clean2, mcx, mcy, r_obs) = isolate_arrow_mask(mask2)?;
    let (mh, mw) = clean2.dim();
    let min_dim = mh.min(mw) as f64;
    let r_exp = 0.305 * min_dim;
    let r_use = r_obs.clamp(0.85 * r_exp, 1.15 * r_exp);
    let max_rad = r_use * (56.0 / 48.0);
    if max_rad < 4.0 {
        return None;
    }

    let jewel_prior = extract_tail_jewel_prior(up2, clean2.view(), mcx, mcy, r_use);
    let tmpls = arrow_polar_templates();
    let mut best: Option<(f64, f64, f64)> = None;

    // 1. Centroid-centered outer-ring polar correlation (translation invariant).
    if let Ok(pol_c) = warp_polar_annulus_u8(
        clean2.view(),
        ARROW_RAD_FULL,
        ARROW_POLAR_BINS,
        (ARROW_RAD_IN, ARROW_RAD_OUT),
        (mcx, mcy),
        max_rad,
        Interpolation::Linear,
    ) && let Some(obs_c) = normalize_arrow_polar(pol_c.view(), 0)
    {
        for tmpl in [&tmpls.genshin_centroid, &tmpls.dart_centroid] {
            if let Some(cand) = score_arrow_polar_template(&obs_c, tmpl, conf_scale, jewel_prior)
                && best.is_none_or(|b| cand.0 > b.0)
            {
                best = Some(cand);
            }
        }
    }

    // 2. Pivot-centered outer-ring polar correlation (recovers exact angle even if a map pin
    //    heavily occludes the nose and shifts the visible mask's centroid backwards).
    let (cx0, cy0) = (mw as f64 * 0.5, mh as f64 * 0.5);
    if let Ok(pol_p) = warp_polar_annulus_u8(
        clean2.view(),
        ARROW_RAD_FULL,
        ARROW_POLAR_BINS,
        (ARROW_RAD_IN, ARROW_RAD_OUT),
        (cx0, cy0),
        max_rad,
        Interpolation::Linear,
    ) && let Some(obs_p) = normalize_arrow_polar(pol_p.view(), 0)
    {
        for tmpl in [&tmpls.genshin_pivot, &tmpls.dart_pivot] {
            if let Some(cand) = score_arrow_polar_template(&obs_p, tmpl, conf_scale, jewel_prior)
                && best.is_none_or(|b| cand.0 > b.0)
            {
                best = Some(cand);
            }
        }
    }

    if let Some((_, deg, conf)) = best {
        return Some((deg, conf));
    }
    if let Some((j_deg, j_str)) = jewel_prior
        && j_str >= 0.70
    {
        return Some((j_deg, (0.62 * j_str * conf_scale).clamp(0.45, 0.75)));
    }
    None
}

pub fn extract_player_arrow_360(minimap_bgr: ArrayView3<'_, u8>) -> ArrowDetection {
    let (h, w, c) = minimap_bgr.dim();
    if (c != 3 && c != 4) || h.min(w) < 20 {
        return ArrowDetection::default();
    }
    let y0 = (0.4 * h as f64).round_ties_even() as usize;
    let y1 = (0.6 * h as f64).round_ties_even() as usize;
    let x0 = (0.4 * w as f64).round_ties_even() as usize;
    let x1 = (0.6 * w as f64).round_ties_even() as usize;
    if y1 <= y0 || x1 <= x0 || y1 > h || x1 > w {
        return ArrowDetection::default();
    }
    let crop = minimap_bgr.slice(s![y0..y1, x0..x1, 0..3]);
    let (ch, cw, _) = crop.dim();
    if ch.min(cw) < 4 {
        return ArrowDetection::default();
    }
    let Ok(up2) = resize_u8_hwc(crop, cw * 2, ch * 2, Interpolation::Linear) else {
        return ArrowDetection::default();
    };
    let (uh, uw, _) = up2.dim();
    let up2_s = up2.as_slice().expect("contiguous up2");
    let total_px = uh * uw;

    // Tier 1: clean electric-cyan player-arrow mask (B >= 205, G >= 175, R <= 45).
    // Excludes teleport waypoints (G ~ 134..170) and water/bloom halos so adjacent map icons
    // cannot bridge into the arrow silhouette while preserving FSR2-dimmed [222, 198, 30] arrows.
    let mut mask_clean = Array2::<u8>::zeros((uh, uw));
    let clean_s = mask_clean.as_slice_mut().expect("contiguous mask_clean");
    let mut clean_nz = 0usize;
    for (dst, px) in clean_s.iter_mut().zip(up2_s.as_chunks::<3>().0) {
        let b = px[0] as i16;
        let g = px[1] as i16;
        let r = px[2] as i16;
        if b >= 205 && g >= 175 && r <= 45 && (b - r) >= 165 && (g - r) >= 135 {
            *dst = 255;
            clean_nz += 1;
        }
    }

    // If clean cyan floods >35% of the crop (e.g. cyan-on-cyan background where only R <= 25
    // isolates the arrow), fall back to the strict rim threshold.
    let mut clean_flooded = false;
    let mut primary_res = if clean_nz * 100 > total_px * 35 {
        clean_flooded = true;
        let mut mask_strict = Array2::<u8>::zeros((uh, uw));
        let strict_s = mask_strict.as_slice_mut().expect("contiguous mask_strict");
        for (dst, px) in strict_s.iter_mut().zip(up2_s.as_chunks::<3>().0) {
            if px[0] > 240 && px[1] > 212 && px[2] <= 25 {
                *dst = 255;
            }
        }
        solve_arrow_from_mask(up2.view(), mask_strict.view(), 1.0)
    } else if clean_nz >= 48 {
        solve_arrow_from_mask(up2.view(), mask_clean.view(), 1.0)
    } else {
        None
    };

    // Tier 2: broad electric-cyan fallback (only when Tier 1 did not succeed or flood).
    if primary_res.is_none() && !clean_flooded {
        let mut mask_broad = Array2::<u8>::zeros((uh, uw));
        let broad_s = mask_broad.as_slice_mut().expect("contiguous mask_broad");
        let mut broad_nz = 0usize;
        for (dst, px) in broad_s.iter_mut().zip(up2_s.as_chunks::<3>().0) {
            let b = px[0] as i16;
            let g = px[1] as i16;
            let r = px[2] as i16;
            if b >= 175 && g >= 125 && r <= 65 && (b - r) >= 130 && (g - r) >= 80 {
                *dst = 255;
                broad_nz += 1;
            }
        }
        if broad_nz >= 24 && broad_nz * 100 <= total_px * 35 {
            primary_res = solve_arrow_from_mask(up2.view(), mask_broad.view(), 0.96);
        }
    }

    if let Some((deg, conf)) = primary_res
        && conf >= 0.60
    {
        return ArrowDetection {
            heading_deg: Some(deg),
            confidence: conf,
        };
    }

    let Ok(hsv) = bgr_to_hsv(up2.view()) else {
        return primary_res.map_or_else(ArrowDetection::default, |(d, c)| ArrowDetection {
            heading_deg: Some(d),
            confidence: c,
        });
    };
    let Ok(mask_hsv) = in_range(hsv.view(), CYAN_HSV_LOWER, CYAN_HSV_UPPER) else {
        return primary_res.map_or_else(ArrowDetection::default, |(d, c)| ArrowDetection {
            heading_deg: Some(d),
            confidence: c,
        });
    };
    let res_hsv = solve_arrow_from_mask(up2.view(), mask_hsv.view(), 0.94);
    if let Some((hd, hc)) = res_hsv
        && primary_res.is_none_or(|(_, bc)| hc > bc)
    {
        return ArrowDetection {
            heading_deg: Some(hd),
            confidence: hc,
        };
    }
    if let Some((bd, bc)) = primary_res {
        return ArrowDetection {
            heading_deg: Some(bd),
            confidence: bc,
        };
    }
    ArrowDetection::default()
}

pub fn extract_player_arrow_360_frame(frame: FrameView<'_>) -> ArrowDetection {
    let Some(arr) = frame_view_to_bgr_or_bgra(frame) else {
        return ArrowDetection::default();
    };
    extract_player_arrow_360(arr.view())
}

#[derive(Debug, Clone)]
pub struct CircularAngleSmoother {
    pub alpha: f64,
    pub deadband_deg: f64,
    sin: Option<f64>,
    cos: Option<f64>,
    wrapped_deg: Option<f64>,
    unwrapped_deg: Option<f64>,
}

impl Default for CircularAngleSmoother {
    fn default() -> Self {
        Self::new(0.35, 1.5).expect("default smoother params valid")
    }
}

impl CircularAngleSmoother {
    pub fn new(alpha: f64, deadband_deg: f64) -> CvResult<Self> {
        if !(alpha > 0.0 && alpha <= 1.0) {
            return Err(CvError::InvalidArgument(format!(
                "alpha must be in (0, 1], got {alpha}"
            )));
        }
        if deadband_deg < 0.0 || !deadband_deg.is_finite() {
            return Err(CvError::InvalidArgument(format!(
                "deadband_deg must be >= 0, got {deadband_deg}"
            )));
        }
        Ok(Self {
            alpha,
            deadband_deg,
            sin: None,
            cos: None,
            wrapped_deg: None,
            unwrapped_deg: None,
        })
    }

    pub fn reset(&mut self) {
        self.sin = None;
        self.cos = None;
        self.wrapped_deg = None;
        self.unwrapped_deg = None;
    }

    pub fn angle(&self) -> Option<f64> {
        self.wrapped_deg
    }

    pub fn unwrapped_angle(&self) -> Option<f64> {
        self.unwrapped_deg
    }

    pub fn update(&mut self, angle_deg: Option<f64>) -> Option<f64> {
        let Some(angle_in) = angle_deg else {
            return self.wrapped_deg;
        };
        let raw = wrap_angle_360(angle_in);
        let (Some(prev_w), Some(prev_s), Some(prev_c), Some(prev_u)) =
            (self.wrapped_deg, self.sin, self.cos, self.unwrapped_deg)
        else {
            let rad = raw.to_radians();
            self.sin = Some(rad.sin());
            self.cos = Some(rad.cos());
            self.wrapped_deg = Some(raw);
            self.unwrapped_deg = Some(raw);
            return self.wrapped_deg;
        };

        let delta = shortest_angle_delta(prev_w, raw);
        if delta.abs() < self.deadband_deg {
            return self.wrapped_deg;
        }

        let rad = raw.to_radians();
        let new_sin = (1.0 - self.alpha) * prev_s + self.alpha * rad.sin();
        let new_cos = (1.0 - self.alpha) * prev_c + self.alpha * rad.cos();
        let norm = new_sin.hypot(new_cos);
        let (s_out, c_out) = if norm < 1e-6 {
            let step_rad = wrap_angle_360(prev_w + self.alpha * delta).to_radians();
            (step_rad.sin(), step_rad.cos())
        } else {
            (new_sin / norm, new_cos / norm)
        };
        self.sin = Some(s_out);
        self.cos = Some(c_out);

        let new_wrapped = wrap_angle_360(s_out.atan2(c_out).to_degrees());
        let step = shortest_angle_delta(prev_w, new_wrapped);
        self.unwrapped_deg = Some(prev_u + step);
        self.wrapped_deg = Some(new_wrapped);
        self.wrapped_deg
    }
}

#[derive(Debug, Clone)]
pub struct CameraHeadingFuser {
    pub jump_deg: f64,
    pub confirm_frames: usize,
    pub confirm_window_s: f64,
    pub stale_s: f64,
    estimate: Option<f64>,
    conf: f64,
    source: FuserSource,
    last_accept_ts: Option<f64>,
    last_trusted_ts: Option<f64>,
    last_pan_cand: Option<(f64, f64)>,
    last_spin_delta: f64,
    last_spin_ts: Option<f64>,
    pending: Vec<(f64, f64)>,
}

impl Default for CameraHeadingFuser {
    fn default() -> Self {
        Self::new(50.0, 3, 0.6, 0.8)
    }
}

impl CameraHeadingFuser {
    pub fn new(jump_deg: f64, confirm_frames: usize, confirm_window_s: f64, stale_s: f64) -> Self {
        Self {
            jump_deg,
            confirm_frames: confirm_frames.max(1),
            confirm_window_s,
            stale_s,
            estimate: None,
            conf: 0.0,
            source: FuserSource::None,
            last_accept_ts: None,
            last_trusted_ts: None,
            last_pan_cand: None,
            last_spin_delta: 0.0,
            last_spin_ts: None,
            pending: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.estimate = None;
        self.conf = 0.0;
        self.source = FuserSource::None;
        self.last_accept_ts = None;
        self.last_trusted_ts = None;
        self.last_pan_cand = None;
        self.last_spin_delta = 0.0;
        self.last_spin_ts = None;
        self.pending.clear();
    }

    pub fn estimate(&self) -> Option<f64> {
        self.estimate
    }

    pub fn confidence(&self) -> f64 {
        self.conf
    }

    pub fn source(&self) -> FuserSource {
        self.source
    }

    /// Returns true when the current lock comes from a trusted cone method updated recently.
    pub fn is_trusted_fresh(&self, now: f64) -> bool {
        matches!(self.source, FuserSource::BgraAlpha | FuserSource::BgrBlend)
            && self.last_trusted_ts.is_some_and(|t| {
                let dt = now - t;
                (0.0..=0.35).contains(&dt)
            })
    }

    /// Refresh freshness timestamps when the minimap ROI is pixel-static so a
    /// stationary player's camera cone heading never ages into a false `Body` snap.
    pub fn touch(&mut self, now: f64) {
        if self.estimate.is_some() {
            self.last_accept_ts = Some(now);
            if matches!(self.source, FuserSource::BgraAlpha | FuserSource::BgrBlend) {
                self.last_trusted_ts = Some(now);
            }
        }
    }

    fn accept(&mut self, angle: f64, conf: f64, ts: f64, source: FuserSource) {
        let wrapped = wrap_angle_360(angle);
        if matches!(source, FuserSource::BgraAlpha | FuserSource::BgrBlend) {
            if let Some(prev) = self.estimate {
                let d = shortest_angle_delta(prev, wrapped);
                if d.abs() >= 1.5 && d.abs() <= 60.0 {
                    self.last_spin_delta = d;
                    self.last_spin_ts = Some(ts);
                } else if d.abs() < 1.5 {
                    self.last_spin_delta *= 0.5;
                }
            }
            self.last_trusted_ts = Some(ts);
        }
        self.estimate = Some(wrapped);
        self.conf = conf;
        self.source = source;
        self.last_accept_ts = Some(ts);
        self.last_pan_cand = None;
        self.pending.clear();
    }

    pub fn update(
        &mut self,
        cam_deg: Option<f64>,
        cam_conf: f64,
        method: ConeMethod,
        body_deg: Option<f64>,
        body_conf: f64,
        now: f64,
    ) -> Option<f64> {
        let trusted = cam_deg.is_some() && cam_conf > 0.0 && method.is_trusted();
        // Reject very weak Tier-3 (HsvAdaptive) terrain-edge noise that drifts ~30-50 deg off.
        let guess = cam_deg.is_some() && cam_conf >= 0.42 && !trusted;
        let body_ok = body_deg.is_some() && body_conf >= 0.5;
        let trusted_fresh = matches!(self.source, FuserSource::BgraAlpha | FuserSource::BgrBlend)
            && self.last_trusted_ts.is_some_and(|t| (now - t) <= 0.35);

        // Never let Tier-3 HsvAdaptive overwrite a fresh trusted lock during a 1-frame blink
        // (prevents snow/cliff edges from jerking a smooth 360-deg spin backwards).
        if let Some(cd) = cam_deg
            && (trusted || (guess && !trusted_fresh))
        {
            let meas = wrap_angle_360(cd);
            let src = FuserSource::from(method);
            match self.estimate {
                None => {
                    if trusted {
                        self.accept(meas, cam_conf, now, src);
                    } else if let Some(bd) = body_deg
                        && body_ok
                        && shortest_angle_delta(bd, meas).abs() <= 35.0
                    {
                        self.accept(meas, cam_conf * 0.8, now, src);
                    } else if cam_conf >= 0.8 {
                        self.accept(meas, cam_conf * 0.7, now, src);
                    }
                }
                Some(est) => {
                    let signed_jump = shortest_angle_delta(est, meas);
                    let jump = signed_jump.abs();
                    let body_close = body_deg
                        .is_some_and(|bd| body_ok && shortest_angle_delta(bd, meas).abs() <= 30.0);
                    let body_agrees = body_deg
                        .is_some_and(|bd| body_ok && shortest_angle_delta(bd, meas).abs() <= 50.0);
                    if trusted {
                        // Tier-1 BgraAlpha reads the exact GPU alpha mask (zero terrain false-jump
                        // risk), so high-confidence alpha flicks can accept immediately.
                        let alpha_instant =
                            method == ConeMethod::BgraAlpha && cam_conf >= 0.85;
                        // Mid-spin backward-glitch guard: while rotating smoothly in one direction
                        // (>= 4 deg step within 0.14s), route a sudden >= 12 deg reversal on RGB
                        // into 2-frame confirmation (16 ms) so single-frame snow glitches never jerk
                        // the cone backwards.
                        let active_spin = !alpha_instant
                            && self.last_spin_delta.abs() >= 4.0
                            && self.last_spin_ts.is_some_and(|t| {
                                let dt = now - t;
                                (0.0..=0.22).contains(&dt)
                            });
                        let sudden_reverse_glitch = active_spin
                            && signed_jump.signum() != self.last_spin_delta.signum()
                            && jump >= 12.0;
                        // Resting-state snow jitter guard: when stationary (not spinning) and
                        // RGB cone confidence is weak/moderate, suppress micro-wobble (<3 deg)
                        // and require 2-frame confirmation for medium jumps (>=12 deg).
                        let weak_resting_wobble =
                            !alpha_instant && !active_spin && cam_conf < 0.65 && jump < 3.0;
                        let weak_resting_glitch =
                            !alpha_instant && !active_spin && cam_conf < 0.68 && jump >= 12.0;

                        if weak_resting_wobble {
                            self.touch(now);
                        } else if (jump <= self.jump_deg
                            && !sudden_reverse_glitch
                            && !weak_resting_glitch)
                            || alpha_instant
                        {
                            self.accept(meas, cam_conf, now, src);
                        } else {
                            // Allow fast continuous trusted mouse pans (2 consecutive monotonic
                            // steps in the same direction) without waiting for static confirmation.
                            let continuous_pan = self.last_pan_cand.is_some_and(|(prev_a, prev_t)| {
                                let dt = now - prev_t;
                                if !(0.0..=0.18).contains(&dt) {
                                    return false;
                                }
                                let d0 = shortest_angle_delta(est, prev_a);
                                let d1 = shortest_angle_delta(prev_a, meas);
                                d0.signum() == d1.signum()
                                    && d1.abs() >= 6.0
                                    && d1.abs() <= 85.0
                            });
                            if continuous_pan {
                                self.accept(meas, cam_conf, now, src);
                            } else {
                                self.last_pan_cand = Some((meas, now));
                                self.pending.retain(|&(a, t)| {
                                    (now - t) <= self.confirm_window_s
                                        && shortest_angle_delta(a, meas).abs() <= 20.0
                                });
                                self.pending.push((meas, now));
                                let need_frames = if cam_conf >= 0.72
                                    || sudden_reverse_glitch
                                    || weak_resting_glitch
                                {
                                    self.confirm_frames.min(2)
                                } else {
                                    self.confirm_frames
                                };
                                if self.pending.len() >= need_frames {
                                    self.accept(meas, cam_conf, now, src);
                                }
                            }
                        }
                    } else {
                        // Tier-3 (HsvAdaptive): allow steady tracking, but block weak (<0.55)
                        // guesses from step-drifting away from a steady body heading.
                        if jump <= 30.0 && (body_close || !body_ok || cam_conf >= 0.55) {
                            self.accept(meas, cam_conf * 0.85, now, src);
                        } else if body_agrees && cam_conf >= 0.65 && jump <= 90.0 {
                            self.accept(meas, cam_conf * 0.8, now, src);
                        }
                    }
                }
            }
        }

        let stale = self
            .last_accept_ts
            .is_none_or(|t| (now - t) > self.stale_s);
        if stale
            && body_ok
            && let Some(bd) = body_deg
        {
            let changed = self.estimate.is_none_or(|e| shortest_angle_delta(e, bd).abs() > 1.0);
            if changed || self.source != FuserSource::Body {
                self.estimate = Some(wrap_angle_360(bd));
                self.conf = (body_conf * 0.5).min(0.5);
                self.source = FuserSource::Body;
                self.last_pan_cand = None;
            }
        }
        self.estimate
    }
}

/// Pure-Rust OpenCV tracker ported from `src/tracker.py` (`CvAutoTrackBridge` & `HybridTracker` omitted).
#[derive(Debug, Clone)]
pub struct PureOpenCVTracker {
    pub cyan_lower: [u8; 3],
    pub cyan_upper: [u8; 3],
    pub yellow_lower: [u8; 3],
    pub yellow_upper: [u8; 3],
    pub last_arrow_confidence: f64,
    pub last_camera_confidence: f64,
    pub last_camera_method: ConeMethod,
}

impl Default for PureOpenCVTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PureOpenCVTracker {
    pub fn new() -> Self {
        Self {
            cyan_lower: CYAN_HSV_LOWER,
            cyan_upper: CYAN_HSV_UPPER,
            yellow_lower: YELLOW_PING_HSV_LOWER,
            yellow_upper: YELLOW_PING_HSV_UPPER,
            last_arrow_confidence: 0.0,
            last_camera_confidence: 0.0,
            last_camera_method: ConeMethod::None,
        }
    }

    pub fn find_player_on_fullscreen_map(
        &mut self,
        map_frame: ArrayView3<'_, u8>,
    ) -> Option<FullscreenPlayerPose> {
        let (h, w, c) = map_frame.dim();
        if h == 0 || w == 0 || c < 3 {
            return None;
        }
        let bgr = if c > 3 {
            map_frame.slice(s![.., .., 0..3])
        } else {
            map_frame
        };
        let hsv = bgr_to_hsv(bgr).ok()?;
        let mask = in_range(hsv.view(), self.cyan_lower, self.cyan_upper).ok()?;
        let mask = morphology_ex_u8(mask.view(), MorphOp::Open, morph_ellipse_3x3().view()).ok()?;
        let contours = find_contours_external(mask.view(), ContourMethod::Simple).ok()?;
        if contours.is_empty() {
            return None;
        }
        let mut best: Option<(&Vec<Point2i>, f64)> = None;
        for c in &contours {
            let area = contour_area(c);
            if area > 20.0 && area < 600.0 && best.is_none_or(|(_, ba)| area > ba) {
                best = Some((c, area));
            }
        }
        let (largest, _) = best?;
        let m = contour_moments(largest);
        if m.m00 == 0.0 {
            return None;
        }
        let cx = m.m10 / m.m00;
        let cy = m.m01 / m.m00;
        let mut best_tip = largest[0];
        let mut best_d2 = -1.0f64;
        for &p in largest {
            let dx = p.x as f64 - cx;
            let dy = p.y as f64 - cy;
            let d2 = dx * dx + dy * dy;
            if d2 > best_d2 {
                best_d2 = d2;
                best_tip = p;
            }
        }
        let angle = wrap_angle_360(
            (best_tip.x as f64 - cx)
                .atan2(-(best_tip.y as f64 - cy))
                .to_degrees(),
        );
        Some(FullscreenPlayerPose {
            pixel_x: cx,
            pixel_y: cy,
            angle_deg: angle,
        })
    }

    pub fn find_player_on_fullscreen_map_frame(
        &mut self,
        frame: FrameView<'_>,
    ) -> Option<FullscreenPlayerPose> {
        let arr = frame_view_to_bgr_or_bgra(frame)?;
        self.find_player_on_fullscreen_map(arr.view())
    }

    pub fn get_minimap_heading(&mut self, minimap_crop: ArrayView3<'_, u8>) -> Option<f64> {
        let det = extract_player_arrow_360(minimap_crop);
        self.last_arrow_confidence = det.confidence;
        det.heading_deg
    }

    pub fn get_minimap_camera_heading(&mut self, minimap_crop: ArrayView3<'_, u8>) -> Option<f64> {
        let det = extract_camera_view_cone(minimap_crop);
        self.last_camera_confidence = det.confidence;
        self.last_camera_method = det.method;
        det.heading_deg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::morph::{
        draw_filled_circle_u8, draw_filled_convex_poly_u8, draw_filled_ellipse_sector_u8,
    };

    fn make_synthetic_cone_bgr(size: usize, heading_cw_deg: f64, alpha_blend: f64) -> Array3<u8> {
        let mut base = Array3::<u8>::zeros((size, size, 3));
        for y in 0..size {
            for x in 0..size {
                let b = (45.0 + 18.0 * (x as f64 / 17.0).sin() + 14.0 * (y as f64 / 23.0).cos())
                    .clamp(10.0, 180.0);
                let g = (75.0 + 22.0 * (x as f64 / 13.0).cos() + 14.0 * (y as f64 / 19.0).sin())
                    .clamp(10.0, 180.0);
                let r = (55.0 + 16.0 * ((x + y) as f64 / 21.0).sin()).clamp(10.0, 180.0);
                base[[y, x, 0]] = b as u8;
                base[[y, x, 1]] = g as u8;
                base[[y, x, 2]] = r as u8;
            }
        }
        let center_cv_deg = heading_cw_deg - 90.0;
        let mut mask = Array2::<u8>::zeros((size, size));
        let c = size as f64 * 0.5;
        draw_filled_ellipse_sector_u8(
            &mut mask,
            c,
            c,
            size as f64 * 0.45,
            size as f64 * 0.45,
            center_cv_deg - 45.0,
            center_cv_deg + 45.0,
            255,
        );
        for y in 0..size {
            for x in 0..size {
                if mask[[y, x]] > 0 {
                    for ch in 0..3 {
                        let v = (1.0 - alpha_blend) * base[[y, x, ch]] as f64 + 255.0 * alpha_blend;
                        base[[y, x, ch]] = v.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
        base
    }

    fn make_synthetic_cone_bgra(size: usize, heading_cw_deg: f64, inverted_alpha: bool) -> Array3<u8> {
        let mut img = Array3::<u8>::zeros((size, size, 4));
        for y in 0..size {
            for x in 0..size {
                img[[y, x, 0]] = 80;
                img[[y, x, 1]] = 110;
                img[[y, x, 2]] = 90;
                img[[y, x, 3]] = if inverted_alpha { 255 } else { 0 };
            }
        }
        let center_cv_deg = heading_cw_deg - 90.0;
        let mut alpha = Array2::<u8>::from_elem((size, size), if inverted_alpha { 255 } else { 0 });
        let c = size as f64 * 0.5;
        draw_filled_ellipse_sector_u8(
            &mut alpha,
            c,
            c,
            size as f64 * 0.45,
            size as f64 * 0.45,
            center_cv_deg - 45.0,
            center_cv_deg + 45.0,
            if inverted_alpha { 160 } else { 95 },
        );
        for y in 0..size {
            for x in 0..size {
                img[[y, x, 3]] = alpha[[y, x]];
            }
        }
        img
    }

    fn make_synthetic_arrow_bgr(
        size: usize,
        heading_cw_deg: f64,
        color_bgr: [u8; 3],
        occlude_nose: bool,
    ) -> Array3<u8> {
        let mut img = Array3::<u8>::from_elem((size, size, 3), 45u8);
        let c = size as f64 * 0.5;
        let su = size as f64 / 212.0;
        let rad = heading_cw_deg.to_radians();
        let fx = rad.sin();
        let fy = -rad.cos();
        let rx = rad.cos();
        let ry = rad.sin();

        let nose = Point2i {
            x: (c + 13.0 * su * fx).round() as i32,
            y: (c + 13.0 * su * fy).round() as i32,
        };
        let w_left = Point2i {
            x: (c - 9.0 * su * fx - 8.5 * su * rx).round() as i32,
            y: (c - 9.0 * su * fy - 8.5 * su * ry).round() as i32,
        };
        let notch = Point2i {
            x: (c - 4.0 * su * fx).round() as i32,
            y: (c - 4.0 * su * fy).round() as i32,
        };
        let w_right = Point2i {
            x: (c - 9.0 * su * fx + 8.5 * su * rx).round() as i32,
            y: (c - 9.0 * su * fy + 8.5 * su * ry).round() as i32,
        };

        let mut mask = Array2::<u8>::zeros((size, size));
        draw_filled_convex_poly_u8(&mut mask, &[nose, w_left, notch], 255);
        draw_filled_convex_poly_u8(&mut mask, &[nose, notch, w_right], 255);
        if occlude_nose {
            let occ_cx = (c + 10.5 * su * fx).round() as i32;
            let occ_cy = (c + 10.5 * su * fy).round() as i32;
            draw_filled_circle_u8(&mut mask, occ_cx, occ_cy, (4.5 * su).round() as i32, 0);
        }
        for y in 0..size {
            for x in 0..size {
                if mask[[y, x]] > 0 {
                    img[[y, x, 0]] = color_bgr[0];
                    img[[y, x, 1]] = color_bgr[1];
                    img[[y, x, 2]] = color_bgr[2];
                }
            }
        }
        img
    }

    #[test]
    fn heading360_angle_helpers_and_nav_relative() {
        assert_eq!(wrap_angle_360(-90.0), 270.0);
        assert_eq!(wrap_angle_360(360.0), 0.0);
        assert_eq!(wrap_angle_360(-0.0), 0.0);
        assert!((shortest_angle_delta(350.0, 10.0) - 20.0).abs() < 1e-9);
        assert!((shortest_angle_delta(10.0, 350.0) + 20.0).abs() < 1e-9);
        assert_eq!(shortest_angle_delta(0.0, 180.0), 180.0);
        assert_eq!(shortest_angle_delta(0.0, 360.0), 0.0);

        let cases = [
            ([0.0, 0.0], [0.0, -100.0], 0.0, 0.0, 100.0),
            ([0.0, 0.0], [100.0, 0.0], 0.0, 90.0, 100.0),
            ([0.0, 0.0], [-100.0, 0.0], 0.0, -90.0, 100.0),
            ([0.0, 0.0], [0.0, 100.0], 0.0, 180.0, 100.0),
            ([100.0, -100.0], [100.0, -100.0], 45.0, 0.0, 0.0),
            ([0.0, 0.0], [100.0, -100.0], 45.0, 0.0, 20_000.0f64.sqrt()),
            ([0.0, 0.0], [0.0, -100.0], 350.0, 10.0, 100.0),
            ([0.0, 0.0], [-100.0, -100.0], 350.0, -35.0, 20_000.0f64.sqrt()),
            ([10.0, 20.0], [10.0, 20.0], 123.0, 0.0, 0.0),
        ];
        for (p, t, v, exp_d, exp_dist) in cases {
            let (d, dist) = compute_nav_relative_angle(p, t, v);
            assert!((d - exp_d).abs() < 1e-6, "got {d} exp {exp_d}");
            assert!((dist - exp_dist).abs() < 1e-6);
        }
    }

    #[test]
    fn circular_angle_smoother_355_to_5_and_reverse() {
        let mut sm = CircularAngleSmoother::new(0.35, 1.5).unwrap();
        assert_eq!(sm.update(Some(355.0)), Some(355.0));
        // Inside deadband
        assert_eq!(sm.update(Some(355.8)), Some(355.0));
        // None holds previous
        assert_eq!(sm.update(None), Some(355.0));
        // Step across 0/360 forward
        for _ in 0..8 {
            sm.update(Some(5.0));
        }
        assert!(sm.unwrapped_angle().unwrap() > 363.0);

        // Reverse across 0/360
        sm.reset();
        sm.update(Some(5.0));
        for _ in 0..8 {
            sm.update(Some(355.0));
        }
        assert!(sm.unwrapped_angle().unwrap() < -3.0);

        // 180-degree cancellation guard
        let mut s2 = CircularAngleSmoother::new(0.5, 0.0).unwrap();
        s2.update(Some(0.0));
        let out = s2.update(Some(180.0)).unwrap();
        assert!(out.is_finite());
    }

    #[test]
    fn camera_heading_fuser_tier3_rejection_and_body_fallback() {
        let mut fuser = CameraHeadingFuser::default();
        // Establish trusted BgraAlpha lock at 42 deg
        let est = fuser.update(Some(42.0), 0.92, ConeMethod::BgraAlpha, Some(40.0), 0.9, 1.0);
        assert_eq!(est, Some(42.0));
        assert_eq!(fuser.source(), FuserSource::BgraAlpha);

        // False 177 deg Tier-3 guess while body is still 40 deg must be rejected
        for i in 1..=6 {
            let e = fuser.update(
                Some(177.0),
                0.70,
                ConeMethod::HsvAdaptive,
                Some(40.0),
                0.9,
                1.0 + i as f64 * 0.05,
            );
            assert!((e.unwrap() - 42.0).abs() < 1e-6);
            assert_eq!(fuser.source(), FuserSource::BgraAlpha);
        }

        // Single-frame trusted jump 250 deg rejected; 3-frame confirmation accepted
        let e_single = fuser.update(Some(250.0), 0.9, ConeMethod::BgrBlend, Some(40.0), 0.9, 1.4);
        assert!((e_single.unwrap() - 42.0).abs() < 1e-6);
        fuser.update(Some(251.0), 0.9, ConeMethod::BgrBlend, Some(40.0), 0.9, 1.45);
        let e_conf = fuser.update(Some(249.0), 0.9, ConeMethod::BgrBlend, Some(40.0), 0.9, 1.50);
        assert!((e_conf.unwrap() - 249.0).abs() < 1e-6);

        // Stale > 0.8s falls back to Body
        let e_stale = fuser.update(None, 0.0, ConeMethod::None, Some(120.0), 0.9, 2.5);
        assert_eq!(e_stale, Some(120.0));
        assert_eq!(fuser.source(), FuserSource::Body);
        assert!(fuser.confidence() <= 0.5);

        // Agreeing Tier-3 within 50 deg of body re-accepted
        let e_agree = fuser.update(
            Some(130.0),
            0.70,
            ConeMethod::HsvAdaptive,
            Some(120.0),
            0.9,
            2.6,
        );
        assert!((e_agree.unwrap() - 130.0).abs() < 1e-6);
        assert_eq!(fuser.source(), FuserSource::HsvAdaptive);
    }

    #[test]
    fn synthetic_cones_and_arrows_parity() {
        // 8 compass directions for BGRA normal & inverted alpha, plus BGR 0.25 blend
        for deg in [0.0f64, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let bgra = make_synthetic_cone_bgra(212, deg, false);
            let det = extract_camera_view_cone(bgra.view());
            assert_eq!(det.method, ConeMethod::BgraAlpha);
            assert!(
                shortest_angle_delta(deg, det.heading_deg.unwrap()).abs() < 1.5,
                "bgra deg={deg} got {:?}",
                det.heading_deg
            );

            let inv = make_synthetic_cone_bgra(212, deg, true);
            let det_inv = extract_camera_view_cone(inv.view());
            assert_eq!(det_inv.method, ConeMethod::BgraAlpha);
            assert!(
                shortest_angle_delta(deg, det_inv.heading_deg.unwrap()).abs() < 1.5,
                "bgra inv deg={deg} got {:?}",
                det_inv.heading_deg
            );

            let bgr = make_synthetic_cone_bgr(212, deg, 0.25);
            let det_bgr = extract_camera_view_cone(bgr.view());
            assert!(det_bgr.method.is_trusted());
            assert!(
                shortest_angle_delta(deg, det_bgr.heading_deg.unwrap()).abs() < 2.5,
                "bgr deg={deg} got {:?}",
                det_bgr.heading_deg
            );
        }

        // Dimmed 0.14 cone
        let dim = make_synthetic_cone_bgr(212, 135.0, 0.14);
        let det_dim = extract_camera_view_cone(dim.view());
        assert!(det_dim.heading_deg.is_some());
        assert!(shortest_angle_delta(135.0, det_dim.heading_deg.unwrap()).abs() < 3.0);

        // Flat snow -> None
        let snow = Array3::<u8>::from_elem((212, 212, 3), 245u8);
        assert_eq!(extract_camera_view_cone(snow.view()).heading_deg, None);

        // 3-tip cyan arrow + FSR2 dimmed HSV + occluded-nose 2-wing + 720p/4K
        for deg in [0.0, 90.0, 180.0, 270.0] {
            let arrow = make_synthetic_arrow_bgr(212, deg, [255, 240, 0], false);
            let ad = extract_player_arrow_360(arrow.view());
            assert!(
                ad.heading_deg.is_some() && shortest_angle_delta(deg, ad.heading_deg.unwrap()).abs() < 2.5,
                "3-tip deg={deg} got {:?}",
                ad
            );

            let fsr = make_synthetic_arrow_bgr(212, deg, [222, 198, 30], false);
            let af = extract_player_arrow_360(fsr.view());
            assert!(
                af.heading_deg.is_some() && shortest_angle_delta(deg, af.heading_deg.unwrap()).abs() < 3.5,
                "fsr deg={deg} got {:?}",
                af
            );
        }

        // Occluded nose 2-wing fallback
        let occ = make_synthetic_arrow_bgr(212, 60.0, [255, 240, 0], true);
        let ao = extract_player_arrow_360(occ.view());
        assert!(
            ao.heading_deg.is_some() && shortest_angle_delta(60.0, ao.heading_deg.unwrap()).abs() < 12.0,
            "occluded nose got {:?}",
            ao
        );

        // 720p (140px) & 4K (424px)
        for sz in [140usize, 424] {
            let a_sz = make_synthetic_arrow_bgr(sz, 135.0, [255, 240, 0], false);
            let det = extract_player_arrow_360(a_sz.view());
            assert!(
                det.heading_deg.is_some() && shortest_angle_delta(135.0, det.heading_deg.unwrap()).abs() < 3.5,
                "sz={sz} got {:?}",
                det
            );
        }

        // Arrow-free -> None
        let empty = Array3::<u8>::from_elem((212, 212, 3), 45u8);
        assert_eq!(extract_player_arrow_360(empty.view()).heading_deg, None);

        // PureOpenCVTracker fullscreen map arrow
        let mut tracker = PureOpenCVTracker::new();
        let full_arrow = make_synthetic_arrow_bgr(212, 90.0, [255, 240, 0], false);
        let pose = tracker
            .find_player_on_fullscreen_map(full_arrow.view())
            .expect("fullscreen pose");
        assert!(shortest_angle_delta(90.0, pose.angle_deg).abs() < 15.0);
    }

    #[test]
    fn player_arrow_bgr_threshold_includes_r_25() {
        // Background [245, 215, 26] and arrow [245, 215, 25] both fall inside CYAN_HSV,
        // so HSV fallback floods the entire crop (`res_hsv = None`). Only `R <= 25` in the
        // primary BGR mask isolates the arrow; `R < 25` would yield `None`.
        let mut img = Array3::<u8>::zeros((212, 212, 3));
        for y in 0..212 {
            for x in 0..212 {
                img[[y, x, 0]] = 245;
                img[[y, x, 1]] = 215;
                img[[y, x, 2]] = 26;
            }
        }
        let arrow = make_synthetic_arrow_bgr(212, 45.0, [245, 215, 25], false);
        for y in 0..212 {
            for x in 0..212 {
                if arrow[[y, x, 0]] == 245 && arrow[[y, x, 1]] == 215 && arrow[[y, x, 2]] == 25 {
                    img[[y, x, 2]] = 25;
                }
            }
        }
        let det = extract_player_arrow_360(img.view());
        let deg = det
            .heading_deg
            .expect("R == 25 arrow must match cv2.threshold(r, 25, 255, THRESH_BINARY_INV)");
        assert!(
            shortest_angle_delta(45.0, deg).abs() < 2.5,
            "expected ~45.0 deg, got {deg}"
        );
        assert!(det.confidence >= 0.65, "conf={}", det.confidence);
    }

    #[test]
    fn extract_camera_view_cone_detects_hdr_linear_blended_cone() {
        // Simulate a WGC HDR linear-light composited minimap (`A = 255`, 25% linear white veil).
        let sz = 212usize;
        let c = sz as f64 * 0.5;
        let lut_lin = lut_pred_linear_u8();
        for target_deg in [0.0, 90.0, 180.0, 270.0] {
            let mut img = Array3::<u8>::zeros((sz, sz, 3));
            for y in 0..sz {
                for x in 0..sz {
                    let dx = x as f64 - c;
                    let dy = y as f64 - c;
                    let r = dx.hypot(dy);
                    let ang = wrap_angle_360(dx.atan2(-dy).to_degrees());
                    let base = [
                        (48 + ((x * 7 + y * 3) % 40)) as u8,
                        (72 + ((x * 5 + y * 11) % 36)) as u8,
                        (54 + ((x * 3 + y * 5) % 34)) as u8,
                    ];
                    let in_cone = r >= 20.0
                        && r <= c * 0.92
                        && shortest_angle_delta(target_deg, ang).abs() <= 45.0;
                    for ch in 0..3 {
                        img[[y, x, ch]] = if in_cone {
                            lut_lin[base[ch] as usize]
                        } else {
                            base[ch]
                        };
                    }
                }
            }
            let det = extract_camera_view_cone(img.view());
            assert_eq!(det.method, ConeMethod::BgrBlend, "deg={target_deg}");
            assert!(
                shortest_angle_delta(target_deg, det.heading_deg.unwrap()).abs() < 2.5,
                "hdr deg={target_deg} got {:?}",
                det.heading_deg
            );
        }
    }

    #[test]
    fn camera_heading_fuser_rejects_tier3_45deg_drift_and_tracks_fast_trusted_pan() {
        let mut fuser = CameraHeadingFuser::default();
        // Player running straight North (0 deg), trusted lock at 0 deg.
        assert_eq!(
            fuser.update(Some(0.0), 0.85, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.0),
            Some(0.0)
        );
        // Noisy Tier-3 HsvAdaptive trying to step-drift 20 -> 40 -> 45 deg off while running straight.
        for (i, deg) in [20.0, 38.0, 45.0, 46.0].into_iter().enumerate() {
            let est = fuser.update(
                Some(deg),
                0.48,
                ConeMethod::HsvAdaptive,
                Some(0.0),
                0.9,
                1.05 + i as f64 * 0.05,
            );
            assert!(
                shortest_angle_delta(0.0, est.unwrap()).abs() < 1.0,
                "weak Tier-3 must not drift cone off straight running: got {est:?}"
            );
        }

        // Fast continuous trusted pan (0 -> 65 -> 120 deg) must track on 2nd monotonic frame!
        let _ = fuser.update(Some(65.0), 0.88, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.30);
        let panned = fuser.update(Some(120.0), 0.88, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.34);
        assert!(
            shortest_angle_delta(120.0, panned.unwrap()).abs() < 1.0,
            "fast continuous trusted pan should not stall: got {panned:?}"
        );
    }

    #[test]
    fn extract_camera_view_cone_radial_gradient_with_bright_distractor() {
        // Real Genshin minimap cone fades radially (~0.24 inner -> ~0.08 outer) and coexists
        // with bright non-cone terrain edges (snow cliffs, buildings). Joint 90-deg white-veil
        // pairing must lock accurately on the cone and ignore the brighter distractor wedge.
        let sz = 212usize;
        let c = sz as f64 * 0.5;
        for target_deg in [23.0, 142.0, 213.0, 263.0] {
            let mut img = Array3::<u8>::zeros((sz, sz, 3));
            let distractor_deg = wrap_angle_360(target_deg + 135.0);
            for y in 0..sz {
                for x in 0..sz {
                    let dx = x as f64 - c;
                    let dy = y as f64 - c;
                    let r = dx.hypot(dy);
                    let r_norm = (r / c).clamp(0.0, 1.0);
                    let ang = wrap_angle_360(dx.atan2(-dy).to_degrees());
                    let mut b = (52.0 + 16.0 * (x as f64 / 15.0).sin() + 10.0 * (y as f64 / 19.0).cos())
                        .clamp(15.0, 170.0);
                    let mut g = (78.0 + 18.0 * (x as f64 / 11.0).cos() + 12.0 * (y as f64 / 17.0).sin())
                        .clamp(20.0, 175.0);
                    let mut red = (60.0 + 14.0 * ((x + y) as f64 / 18.0).sin()).clamp(15.0, 170.0);
                    // Bright single-sided / narrow 35-deg distractor cliff edge (not 90 deg!)
                    if r >= 22.0
                        && r <= c * 0.90
                        && shortest_angle_delta(distractor_deg, ang).abs() <= 17.5
                    {
                        b = (b * 0.75 + 255.0 * 0.25).min(255.0);
                        g = (g * 0.75 + 255.0 * 0.25).min(255.0);
                        red = (red * 0.75 + 255.0 * 0.25).min(255.0);
                    }
                    // Radial-gradient 90-deg white-veil camera cone: alpha 0.23 -> 0.08
                    if r >= 20.0
                        && r <= c * 0.92
                        && shortest_angle_delta(target_deg, ang).abs() <= 45.0
                    {
                        let a = (0.24 - 0.16 * r_norm).clamp(0.07, 0.24);
                        b = b * (1.0 - a) + 255.0 * a;
                        g = g * (1.0 - a) + 255.0 * a;
                        red = red * (1.0 - a) + 255.0 * a;
                    }
                    img[[y, x, 0]] = b.round().clamp(0.0, 255.0) as u8;
                    img[[y, x, 1]] = g.round().clamp(0.0, 255.0) as u8;
                    img[[y, x, 2]] = red.round().clamp(0.0, 255.0) as u8;
                }
            }
            let det = extract_camera_view_cone(img.view());
            assert_eq!(det.method, ConeMethod::BgrBlend, "target_deg={target_deg}");
            let got = det.heading_deg.expect("radial-gradient cone heading");
            assert!(
                shortest_angle_delta(target_deg, got).abs() < 2.5,
                "target_deg={target_deg} got={got}"
            );
        }
    }

    #[test]
    fn camera_heading_fuser_touch_prevents_static_body_snap_and_2frame_high_conf_flick() {
        let mut fuser = CameraHeadingFuser::default();
        // Trusted lock at 260 deg while body arrow faces 90 deg (player looking sideways).
        assert_eq!(
            fuser.update(Some(260.0), 0.86, ConeMethod::BgrBlend, Some(90.0), 0.9, 1.0),
            Some(260.0)
        );
        // Player stands still for 3 seconds (static ROI -> `touch` called every tick).
        for step in 1..=30 {
            fuser.touch(1.0 + step as f64 * 0.1);
        }
        assert_eq!(fuser.estimate(), Some(260.0));
        assert_eq!(fuser.source(), FuserSource::BgrBlend);
        // Next normal tick with `None` immediately after touch must NOT snap to Body (90 deg).
        let after = fuser.update(None, 0.0, ConeMethod::None, Some(90.0), 0.9, 4.1);
        assert_eq!(after, Some(260.0));
        assert_eq!(fuser.source(), FuserSource::BgrBlend);

        // Rapid high-confidence (>0.72) 180-deg camera flick confirms on 2nd frame (not 3rd).
        let f1 = fuser.update(Some(80.0), 0.84, ConeMethod::BgrBlend, Some(90.0), 0.9, 4.15);
        assert_eq!(f1, Some(260.0), "single-frame glitch must still be rejected");
        let f2 = fuser.update(Some(81.0), 0.84, ConeMethod::BgrBlend, Some(90.0), 0.9, 4.19);
        assert_eq!(f2, Some(81.0), "2nd high-conf frame must confirm fast flick");
    }

    #[test]
    fn camera_heading_fuser_blocks_mid_spin_backward_glitch_and_fresh_tier3_override() {
        let mut fuser = CameraHeadingFuser::default();
        // Establish smooth clockwise spin: 100 -> 110 -> 120 deg at 55 Hz (~18ms steps).
        assert_eq!(
            fuser.update(Some(100.0), 0.85, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.000),
            Some(100.0)
        );
        assert_eq!(
            fuser.update(Some(110.0), 0.85, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.018),
            Some(110.0)
        );
        assert_eq!(
            fuser.update(Some(120.0), 0.85, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.036),
            Some(120.0)
        );

        // Single-frame high-confidence Tier-3 HsvAdaptive (conf=0.78, 22 deg backwards) must be
        // ignored while trusted lock is fresh (<= 0.35s).
        let t3 = fuser.update(Some(98.0), 0.78, ConeMethod::HsvAdaptive, Some(0.0), 0.9, 1.054);
        assert_eq!(t3, Some(120.0), "fresh trusted lock must reject Tier-3 backward pull");

        // Single-frame RGB backward glitch (-24 deg against active CW spin) must NOT jerk cone back.
        let rev1 = fuser.update(Some(96.0), 0.80, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.072);
        assert_eq!(rev1, Some(120.0), "1-frame backward glitch mid-spin must be held");

        // Next frame continues smooth CW spin -> accepts immediately with zero jerk!
        let cont = fuser.update(Some(132.0), 0.86, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.090);
        assert_eq!(cont, Some(132.0));

        // Genuine user reversal (2 consecutive CCW frames: 112 -> 104) confirms in 2 frames (~18ms).
        let g1 = fuser.update(Some(112.0), 0.84, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.108);
        assert_eq!(g1, Some(132.0));
        let g2 = fuser.update(Some(104.0), 0.84, ConeMethod::BgrBlend, Some(0.0), 0.9, 1.126);
        assert_eq!(g2, Some(104.0), "genuine 2-frame reversal must confirm cleanly");
    }

    fn make_real_genshin_two_tone_arrow_bgr(
        size: usize,
        heading_cw_deg: f64,
        with_nearby_pins: bool,
    ) -> Array3<u8> {
        // Reproduces real Genshin minimap arrow: bright rim [255,219,0], darker cyan core
        // [255,201,0] (which hollowed out the legacy G>212 BGR mask!), and protruding tail tab.
        let mut img = Array3::<u8>::from_elem((size, size, 3), 90u8);
        let c = size as f64 * 0.5;
        let r = 0.0608 * size as f64;
        let rad = heading_cw_deg.to_radians();
        let fx = rad.sin();
        let fy = -rad.cos();
        let rx = rad.cos();
        let ry = rad.sin();
        let pt = |lx: f64, ly: f64| Point2i {
            x: (c + (lx * rx - ly * fx) * r).round() as i32,
            y: (c + (lx * ry - ly * fy) * r).round() as i32,
        };
        let mut outer = Array2::<u8>::zeros((size, size));
        draw_filled_convex_poly_u8(&mut outer, &[pt(0.0, -1.00), pt(-0.84, 0.51), pt(0.0, 0.51)], 255);
        draw_filled_convex_poly_u8(&mut outer, &[pt(0.0, -1.00), pt(0.0, 0.51), pt(0.84, 0.51)], 255);
        draw_filled_convex_poly_u8(&mut outer, &[pt(-0.31, 0.49), pt(0.0, 0.70), pt(0.31, 0.49)], 255);

        let mut inner = Array2::<u8>::zeros((size, size));
        draw_filled_convex_poly_u8(&mut inner, &[pt(0.0, -0.72), pt(-0.56, 0.34), pt(0.56, 0.34)], 255);

        for y in 0..size {
            for x in 0..size {
                if inner[[y, x]] != 0 {
                    img[[y, x, 0]] = 255;
                    img[[y, x, 1]] = 201;
                    img[[y, x, 2]] = 0;
                } else if outer[[y, x]] != 0 {
                    img[[y, x, 0]] = 255;
                    img[[y, x, 1]] = 219;
                    img[[y, x, 2]] = 0;
                }
            }
        }
        if with_nearby_pins {
            // Nearby bright cyan waypoint diamond at edge of 0.4..0.6 crop + white star clipping one side.
            let wp_x = (c - 0.085 * size as f64).round() as i32;
            let wp_y = (c - 0.085 * size as f64).round() as i32;
            for dy in -5i32..=5 {
                for dx in -5i32..=5 {
                    if dx.abs() + dy.abs() <= 5 {
                        let px = (wp_x + dx) as usize;
                        let py = (wp_y + dy) as usize;
                        if px < size && py < size {
                            img[[py, px, 0]] = 255;
                            img[[py, px, 1]] = 225;
                            img[[py, px, 2]] = 10;
                        }
                    }
                }
            }
            // White pin biting a small notch out of the right side of the arrow.
            let bx = (c + (0.55 * rx - (-0.05) * fx) * r).round() as i32;
            let by = (c + (0.55 * ry - (-0.05) * fy) * r).round() as i32;
            let mut bite = Array2::<u8>::zeros((size, size));
            draw_filled_circle_u8(&mut bite, bx, by, (0.24 * r).round().max(2.0) as i32, 255);
            for y in 0..size {
                for x in 0..size {
                    if bite[[y, x]] != 0 {
                        img[[y, x, 0]] = 248;
                        img[[y, x, 1]] = 248;
                        img[[y, x, 2]] = 248;
                    }
                }
            }
        }
        img
    }

    #[test]
    fn real_two_tone_genshin_arrow_with_pins_and_all_occluded_angles() {
        for deg in [0.0f64, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let clean = make_real_genshin_two_tone_arrow_bgr(290, deg, false);
            let det = extract_player_arrow_360(clean.view());
            let est = det.heading_deg.unwrap_or_else(|| panic!("two-tone deg={deg} returned None"));
            assert!(
                shortest_angle_delta(deg, est).abs() < 4.0,
                "two-tone clean deg={deg} got={est:.2} conf={:.2}",
                det.confidence
            );
            assert!(det.confidence >= 0.65, "deg={deg} low conf={:.2}", det.confidence);

            let pinned = make_real_genshin_two_tone_arrow_bgr(290, deg, true);
            let det_p = extract_player_arrow_360(pinned.view());
            let est_p = det_p
                .heading_deg
                .unwrap_or_else(|| panic!("two-tone pinned deg={deg} returned None"));
            assert!(
                shortest_angle_delta(deg, est_p).abs() < 12.0,
                "two-tone pinned deg={deg} got={est_p:.2} conf={:.2}",
                det_p.confidence
            );

            let occ = make_synthetic_arrow_bgr(212, deg, [255, 240, 0], true);
            let det_o = extract_player_arrow_360(occ.view());
            let est_o = det_o
                .heading_deg
                .unwrap_or_else(|| panic!("occluded nose deg={deg} returned None"));
            assert!(
                shortest_angle_delta(deg, est_o).abs() < 8.0,
                "occluded nose deg={deg} got={est_o:.2} conf={:.2}",
                det_o.confidence
            );
        }
    }

    #[test]
    fn inner_cone_sector_gate_ignores_snowflakes_and_lighting_but_detects_real_turn() {
        let base = make_synthetic_cone_bgr(290, 110.0, 0.22);
        let p_ref = inner_cone_sector_profile(base.view()).expect("ref profile");

        // Add sparse bright snowflakes + uniform cloud lighting shift (-4) + pulsing rim icon.
        let mut snowy = base.clone();
        for y in 0..290 {
            for x in 0..290 {
                for ch in 0..3 {
                    snowy[[y, x, ch]] = snowy[[y, x, ch]].saturating_sub(4);
                }
            }
        }
        // Scatter 18 falling 3x3 white snowflakes across the minimap.
        for k in 0..18usize {
            let sx = 45 + (k * 37) % 200;
            let sy = 45 + (k * 53) % 200;
            for dy in 0..3 {
                for dx in 0..3 {
                    snowy[[sy + dy, sx + dx, 0]] = 250;
                    snowy[[sy + dy, sx + dx, 1]] = 250;
                    snowy[[sy + dy, sx + dx, 2]] = 250;
                }
            }
        }
        // Bright pulsing quest diamond at outer rim (9 o'clock, r > 70% R).
        for y in 125..165 {
            for x in 10..42 {
                snowy[[y, x, 0]] = 255;
                snowy[[y, x, 1]] = 220;
                snowy[[y, x, 2]] = 160;
            }
        }
        let p_snow = inner_cone_sector_profile(snowy.view()).expect("snow profile");
        let snow_diff = inner_cone_sector_top3_diff(&p_snow, &p_ref);
        assert!(
            snow_diff < CONE_STATIC_SECTOR_TOP3_THRESH,
            "stationary snow+lighting+rim icon should stay below threshold: got {snow_diff:.2}"
        );

        // Real 4-deg camera rotation must trigger immediately.
        let rotated = make_synthetic_cone_bgr(290, 114.0, 0.22);
        let p_rot = inner_cone_sector_profile(rotated.view()).expect("rot profile");
        let rot_diff = inner_cone_sector_top3_diff(&p_rot, &p_ref);
        assert!(
            rot_diff >= CONE_STATIC_SECTOR_TOP3_THRESH,
            "4-deg camera rotation must exceed threshold: got {rot_diff:.2}"
        );
    }

    #[test]
    fn camera_heading_fuser_suppresses_weak_resting_snow_jitter() {
        let mut fuser = CameraHeadingFuser::default();
        assert_eq!(
            fuser.update(Some(200.0), 0.85, ConeMethod::BgrBlend, Some(10.0), 0.9, 1.0),
            Some(200.0)
        );
        // Weak resting micro-wobble (<3 deg, conf < 0.65) holds exact estimate & keeps lock fresh.
        for step in 1..=20 {
            let noisy = if step % 2 == 0 { 202.4 } else { 197.8 };
            let est = fuser.update(
                Some(noisy),
                0.58,
                ConeMethod::BgrBlend,
                Some(10.0),
                0.9,
                1.0 + step as f64 * 0.1,
            );
            assert_eq!(est, Some(200.0));
            assert_eq!(fuser.source(), FuserSource::BgrBlend);
        }
        // Single-frame weak resting jump (28 deg, conf 0.60) held; 2nd frame confirms.
        let g1 = fuser.update(Some(228.0), 0.60, ConeMethod::BgrBlend, Some(10.0), 0.9, 3.10);
        assert_eq!(g1, Some(200.0));
        let g2 = fuser.update(Some(229.0), 0.60, ConeMethod::BgrBlend, Some(10.0), 0.9, 3.15);
        assert_eq!(g2, Some(229.0));
    }

    #[test]
    fn real_geometry_arrow_with_tail_jewel_and_waypoint_occlusion_no_120_flips() {
        // Real Genshin 291x291 minimap arrow: symmetric triangle + protruding tab, uniform cyan
        // body [254, 227, 1], bright tail jewel [255, 250, 1] strictly opposite nose (+0.28 r),
        // and an adjacent Teleport Waypoint cyan diamond [217, 143, 35] touching one wing
        // (plus a light notch on the nose tip). Without clean G>=175 isolation + tail-jewel prior,
        // the broadened mask attaches the waypoint lump and flips +/-120 deg.
        let sz = 291usize;
        let c = sz as f64 * 0.5;
        let r = 0.0608 * sz as f64;
        for deg in [0.0f64, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            let mut img = Array3::<u8>::from_elem((sz, sz, 3), 85u8);
            let rad = deg.to_radians();
            let fx = rad.sin();
            let fy = -rad.cos();
            let rx = rad.cos();
            let ry = rad.sin();
            let pt = |lx: f64, ly: f64| Point2i {
                x: (c + (lx * rx - ly * fx) * r).round() as i32,
                y: (c + (lx * ry - ly * fy) * r).round() as i32,
            };

            // 1. Adjacent waypoint cyan diamond ([217, 143, 35] with white center) touching the
            //    left wing outer edge (~lx=-0.88, ly=+0.10) — drawn FIRST, behind arrow.
            let wp_x = (c + (-0.88 * rx - 0.10 * fx) * r).round() as i32;
            let wp_y = (c + (-0.88 * ry - 0.10 * fy) * r).round() as i32;
            for dy in -6i32..=6 {
                for dx in -6i32..=6 {
                    if dx.abs() + dy.abs() <= 6 {
                        let px = (wp_x + dx) as usize;
                        let py = (wp_y + dy) as usize;
                        if px < sz && py < sz {
                            img[[py, px, 0]] = 217;
                            img[[py, px, 1]] = 143;
                            img[[py, px, 2]] = 35;
                        }
                    }
                }
            }

            // 2. Player arrow polygon + rear tail jewel
            let mut body_mask = Array2::<u8>::zeros((sz, sz));
            draw_filled_convex_poly_u8(
                &mut body_mask,
                &[pt(0.0, -1.00), pt(-0.84, 0.51), pt(0.0, 0.51)],
                255,
            );
            draw_filled_convex_poly_u8(
                &mut body_mask,
                &[pt(0.0, -1.00), pt(0.0, 0.51), pt(0.84, 0.51)],
                255,
            );
            draw_filled_convex_poly_u8(
                &mut body_mask,
                &[pt(-0.31, 0.49), pt(0.0, 0.70), pt(0.31, 0.49)],
                255,
            );

            let mut jewel_mask = Array2::<u8>::zeros((sz, sz));
            let jpt = pt(0.0, 0.28);
            draw_filled_circle_u8(&mut jewel_mask, jpt.x, jpt.y, (0.28 * r).round().max(3.0) as i32, 255);

            for y in 0..sz {
                for x in 0..sz {
                    if body_mask[[y, x]] != 0 {
                        if jewel_mask[[y, x]] != 0 {
                            img[[y, x, 0]] = 255;
                            img[[y, x, 1]] = 250;
                            img[[y, x, 2]] = 1;
                        } else {
                            img[[y, x, 0]] = 254;
                            img[[y, x, 1]] = 227;
                            img[[y, x, 2]] = 1;
                        }
                    }
                }
            }

            // Small pin highlight trimming only the outer nose tip slightly
            let tip = pt(-0.08, -0.92);
            let mut tip_bite = Array2::<u8>::zeros((sz, sz));
            draw_filled_circle_u8(&mut tip_bite, tip.x, tip.y, (0.22 * r).round().max(2.0) as i32, 255);
            for y in 0..sz {
                for x in 0..sz {
                    if tip_bite[[y, x]] != 0 {
                        img[[y, x, 0]] = 248;
                        img[[y, x, 1]] = 248;
                        img[[y, x, 2]] = 248;
                    }
                }
            }

            let det = extract_player_arrow_360(img.view());
            let got = det
                .heading_deg
                .unwrap_or_else(|| panic!("deg={deg} returned None"));
            let err = shortest_angle_delta(deg, got).abs();
            assert!(
                err <= 8.0,
                "waypoint-adjacent tail-jewel arrow deg={deg} flipped/drifted: got={got:.2} (err={err:.2}, conf={:.2})",
                det.confidence
            );
        }
    }

    #[test]
    fn camera_cone_dark_terrain_and_static_distractor_smooth_sweep() {
        let sz = 291usize;
        let c = sz as f64 * 0.5;

        // Case A: True cone overlaid on darker low-saturation terrain while flanking terrain
        // 55..110 deg away is brighter (global in_v < out_v), yet local cone edges still brighten.
        let target_a = 120.0f64;
        let mut img_a = Array3::<u8>::zeros((sz, sz, 3));
        for y in 0..sz {
            for x in 0..sz {
                let dx = x as f64 - c;
                let dy = y as f64 - c;
                let r = dx.hypot(dy);
                let r_norm = (r / c).clamp(0.0, 1.0);
                let ang = wrap_angle_360(dx.atan2(-dy).to_degrees());
                let d_targ = shortest_angle_delta(target_a, ang).abs();
                let mut base = if d_targ <= 56.0 {
                    // Dark underlying corridor + immediate edge border
                    [42.0f64, 49.0, 44.0]
                } else {
                    // Brighter flanking plateau (out_v > in_v globally, low saturation)
                    [78.0f64, 86.0, 80.0]
                };
                if r >= 20.0 && r <= c * 0.92 && d_targ <= 45.0 {
                    let a = (0.20 - 0.12 * r_norm).clamp(0.06, 0.20);
                    for v in &mut base {
                        *v = *v * (1.0 - a) + 255.0 * a;
                    }
                }
                for (ch, &v) in base.iter().enumerate() {
                    img_a[[y, x, ch]] = v.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        let det_a = extract_camera_view_cone_with_prior(img_a.view(), Some(108.0));
        assert_eq!(det_a.method, ConeMethod::BgrBlend, "dark terrain case: {det_a:?}");
        let err_a = shortest_angle_delta(target_a, det_a.heading_deg.unwrap()).abs();
        assert!(err_a <= 6.0, "dark terrain cone err={err_a:.2} (det={det_a:?})");

        // Case B: Moderate cone (alpha ~ 0.14) near prior 275 deg competing with a stronger
        // static bright 90-deg terrain block 125 deg away (around 33 deg).
        let target_b = 268.0f64;
        let distractor_b = 33.0f64;
        let mut img_b = Array3::<u8>::zeros((sz, sz, 3));
        for y in 0..sz {
            for x in 0..sz {
                let dx = x as f64 - c;
                let dy = y as f64 - c;
                let r = dx.hypot(dy);
                let r_norm = (r / c).clamp(0.0, 1.0);
                let ang = wrap_angle_360(dx.atan2(-dy).to_degrees());
                let mut base = [
                    48.0 + 12.0 * (x as f64 / 15.0).sin(),
                    72.0 + 12.0 * (y as f64 / 17.0).cos(),
                    54.0 + 10.0 * ((x + y) as f64 / 19.0).sin(),
                ];
                if r >= 20.0
                    && r <= c * 0.92
                    && shortest_angle_delta(distractor_b, ang).abs() <= 45.0
                {
                    // Static bright terrain block mimicking a 24% white veil
                    let a = 0.24 - 0.10 * r_norm;
                    for v in &mut base {
                        *v = *v * (1.0 - a) + 255.0 * a;
                    }
                }
                if r >= 20.0
                    && r <= c * 0.92
                    && shortest_angle_delta(target_b, ang).abs() <= 45.0
                {
                    let a = (0.15 - 0.07 * r_norm).clamp(0.06, 0.15);
                    for v in &mut base {
                        *v = *v * (1.0 - a) + 255.0 * a;
                    }
                }
                for (ch, &v) in base.iter().enumerate() {
                    img_b[[y, x, ch]] = v.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        let det_b = extract_camera_view_cone_with_prior(img_b.view(), Some(275.0));
        assert_eq!(det_b.method, ConeMethod::BgrBlend, "distractor case: {det_b:?}");
        let err_b = shortest_angle_delta(target_b, det_b.heading_deg.unwrap()).abs();
        assert!(
            err_b <= 8.0,
            "cone should track 268 deg near prior 275 instead of jumping to 33 deg distractor: got {:?} (err={err_b:.2})",
            det_b
        );
    }
}
