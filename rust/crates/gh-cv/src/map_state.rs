//! Visual 3-Anchor Map (M) detector, 4-state FSM, and RGB64 Bhattacharyya scene-cut (`src/map_state.py`).

use crate::ops::color::{
    bhattacharyya_distance as ops_bhattacharyya_distance, bgr_to_gray, frame_view_to_bgr,
    rgb_hist64_sampled,
};
use crate::ops::ncc::{match_template_u8_hwc, min_max_loc_f32};
use crate::ops::resize::resize_u8_hwc;
use crate::ops::resize::resize_u8;
use crate::ops::{Interpolation, TemplateMatchMode};
use crate::zoom_slider::{ZoomSliderDetector, ZoomSliderResult};
use gh_core::assets::resolve_data_asset;
use gh_core::frame::{FrameView, RectI32};
use gh_core::npz::NpzArchive;
use ndarray::{Array2, Array3, ArrayView3, s};
use std::collections::HashMap;
use std::path::Path;

pub const ANCHOR_THRESHOLD: f64 = 0.82;
pub const SLIDER_THRESHOLD: f64 = 0.88;
pub const SLIDER_STRICT_MATCH: f64 = 0.975;
pub const SLIDER_CCOEFF_THRESHOLD: f64 = 0.65;
pub const CLOSE_DEBOUNCE_FRAMES: usize = 3;
pub const SCENE_CUT_THRESHOLD: f64 = 0.40;
pub const MAX_SETTLING_FRAMES: usize = 20;
pub const MAX_RETRIES: usize = 2;
pub const CLOSE_PROBE_DEFAULT_THRESHOLD: f64 = 0.78;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MapState {
    #[default]
    Closed,
    Settling,
    SyncedIdle,
    Panning,
}

impl MapState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::Settling => "SETTLING",
            Self::SyncedIdle => "SYNCED_IDLE",
            Self::Panning => "PANNING",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MapOpenCheck {
    pub is_open: bool,
    pub anchor_count: u8,
    pub slider_strict_ok: bool,
    pub slider_ccoeff: f64,
    pub slider_res: Option<ZoomSliderResult>,
    pub gear_score: f64,
    pub close_score: f64,
}

#[derive(Debug, Clone)]
pub struct MapStepResult {
    pub state: MapState,
    pub should_auto_sync: bool,
    pub scene_cut: Option<bool>,
    pub bhatt_dist: Option<f64>,
    pub check: MapOpenCheck,
}

#[derive(Debug, Clone, Default)]
struct ScaledAnchorPair {
    gear: Option<Array3<u8>>,
    close: Option<Array3<u8>>,
}

pub struct MapScreenStateMachine {
    tpl_gear: Option<Array3<u8>>,
    tpl_close_x: Option<Array3<u8>>,
    scaled_cache: HashMap<usize, ScaledAnchorPair>,
    state: MapState,
    prev_thumb: Option<Array2<u8>>,
    prev_hud: Option<Vec<u8>>,
    synced_thumb: Option<Array2<u8>>,
    prev_hist64: Option<[f32; 64]>,
    stable_frames: usize,
    closed_frames: usize,
    settling_frames: usize,
    settling_drag_frames: usize,
    retries: usize,
}

impl Default for MapScreenStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

fn load_template_u8_3d(archive: &NpzArchive, key: &str) -> Option<Array3<u8>> {
    let arr = archive.get(key)?;
    let shape = arr.shape();
    if shape.len() != 3 || shape[0] == 0 || shape[1] == 0 || shape[2] < 3 {
        return None;
    }
    let data = arr.as_u8().ok()?.to_vec();
    let full = Array3::from_shape_vec((shape[0], shape[1], shape[2]), data).ok()?;
    if shape[2] == 3 {
        Some(full)
    } else {
        Some(full.slice(s![.., .., 0..3]).to_owned())
    }
}

fn mean_absdiff_2d(a: &Array2<u8>, b: &Array2<u8>) -> f64 {
    if a.dim() != b.dim() || a.is_empty() {
        return 0.0;
    }
    let mut sum = 0u64;
    let mut count = 0usize;
    for (&va, &vb) in a.iter().zip(b.iter()) {
        sum += va.abs_diff(vb) as u64;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    }
}

fn mean_absdiff_slice(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let sum: u64 = a
        .iter()
        .zip(b.iter())
        .map(|(&va, &vb)| va.abs_diff(vb) as u64)
        .sum();
    sum as f64 / a.len() as f64
}

impl MapScreenStateMachine {
    pub fn new() -> Self {
        let path = resolve_data_asset("ui_anchors_v4.npz");
        Self::with_anchors_path(Some(&path))
    }

    pub fn with_anchors_path(anchors_path: Option<&Path>) -> Self {
        let mut tpl_gear = None;
        let mut tpl_close_x = None;
        if let Some(p) = anchors_path
            && p.is_file()
            && let Ok(archive) = NpzArchive::read_file(p)
        {
            tpl_gear = load_template_u8_3d(&archive, "tpl_gear");
            tpl_close_x = load_template_u8_3d(&archive, "tpl_close_x");
        }
        Self {
            tpl_gear,
            tpl_close_x,
            scaled_cache: HashMap::new(),
            state: MapState::Closed,
            prev_thumb: None,
            prev_hud: None,
            synced_thumb: None,
            prev_hist64: None,
            stable_frames: 0,
            closed_frames: 0,
            settling_frames: 0,
            settling_drag_frames: 0,
            retries: 0,
        }
    }

    /// Clones only the immutable UI anchor templates into a fresh `Closed` state machine.
    pub fn clone_templates_fresh(&self) -> Self {
        Self {
            tpl_gear: self.tpl_gear.clone(),
            tpl_close_x: self.tpl_close_x.clone(),
            scaled_cache: HashMap::new(),
            state: MapState::Closed,
            prev_thumb: None,
            prev_hud: None,
            synced_thumb: None,
            prev_hist64: None,
            stable_frames: 0,
            closed_frames: 0,
            settling_frames: 0,
            settling_drag_frames: 0,
            retries: 0,
        }
    }

    pub fn reset(&mut self) {
        self.state = MapState::Closed;
        self.prev_thumb = None;
        self.prev_hud = None;
        self.synced_thumb = None;
        self.prev_hist64 = None;
        self.stable_frames = 0;
        self.closed_frames = 0;
        self.settling_frames = 0;
        self.settling_drag_frames = 0;
        self.retries = 0;
    }

    pub fn state(&self) -> MapState {
        self.state
    }

    pub fn retries(&self) -> usize {
        self.retries
    }

    pub fn has_prev_hist64(&self) -> bool {
        self.prev_hist64.is_some()
    }

    pub fn has_anchor_templates(&self) -> bool {
        self.tpl_gear.is_some() && self.tpl_close_x.is_some()
    }

    fn get_scaled_anchors(&mut self, scale_ui: f64) -> (Option<Array3<u8>>, Option<Array3<u8>>) {
        let sz = ((28.0 * scale_ui).round_ties_even() as isize).max(12) as usize;
        if let Some(cached) = self.scaled_cache.get(&sz) {
            return (cached.gear.clone(), cached.close.clone());
        }
        let interp = if sz < 56 {
            Interpolation::Area
        } else {
            Interpolation::Linear
        };
        let gear = self.tpl_gear.as_ref().and_then(|t| {
            if t.dim().0 == sz && t.dim().1 == sz {
                Some(t.clone())
            } else {
                resize_u8_hwc(t.view(), sz, sz, interp).ok()
            }
        });
        let close = self.tpl_close_x.as_ref().and_then(|t| {
            if t.dim().0 == sz && t.dim().1 == sz {
                Some(t.clone())
            } else {
                resize_u8_hwc(t.view(), sz, sz, interp).ok()
            }
        });
        self.scaled_cache.insert(
            sz,
            ScaledAnchorPair {
                gear: gear.clone(),
                close: close.clone(),
            },
        );
        (gear, close)
    }

    pub fn get_slider_probe_rect(win_left: i32, win_top: i32, win_w: i32, win_h: i32) -> RectI32 {
        if win_w <= 0 || win_h <= 0 {
            return RectI32 {
                x: win_left,
                y: win_top,
                width: 0,
                height: 0,
            };
        }
        let x0 = ((win_w as f64 * 0.012) as i32).max(0);
        let x1 = win_w.min((win_w as f64 * 0.062) as i32);
        let y0 = (win_h as f64 * 0.22) as i32;
        let y1 = (win_h as f64 * 0.68) as i32;
        RectI32 {
            x: win_left + x0,
            y: win_top + y0,
            width: (x1 - x0).max(0),
            height: (y1 - y0).max(0),
        }
    }

    pub fn is_slider_probe(
        slider_roi_bgr: Option<ArrayView3<'_, u8>>,
        frame_w: usize,
        frame_h: usize,
    ) -> (bool, f64) {
        if frame_h < 500 || frame_w < 800 {
            return (false, 0.0);
        }
        let Some(roi) = slider_roi_bgr else {
            return (false, 0.0);
        };
        let x_start = (frame_w as f64 * 0.012) as usize;
        let y_start = (frame_h as f64 * 0.22) as usize;
        let Some(res) = ZoomSliderDetector::detect_in_roi(roi, frame_w, frame_h, x_start, y_start)
        else {
            return (false, 0.0);
        };
        let ok = res.match_score >= SLIDER_STRICT_MATCH && res.ccoeff >= SLIDER_CCOEFF_THRESHOLD;
        (ok, res.match_score)
    }

    pub fn is_slider_probe_frame(
        slider_roi: Option<FrameView<'_>>,
        frame_w: usize,
        frame_h: usize,
    ) -> (bool, f64) {
        let Some(fv) = slider_roi else {
            return (false, 0.0);
        };
        let Ok(bgr) = frame_view_to_bgr(fv) else {
            return (false, 0.0);
        };
        Self::is_slider_probe(Some(bgr.view()), frame_w, frame_h)
    }

    pub fn get_close_probe_rect(win_left: i32, win_top: i32, win_w: i32, win_h: i32) -> RectI32 {
        if win_w <= 0 || win_h <= 0 {
            return RectI32 {
                x: win_left,
                y: win_top,
                width: 0,
                height: 0,
            };
        }
        let x_off = (0.90 * win_w as f64) as i32;
        RectI32 {
            x: win_left + x_off,
            y: win_top,
            width: win_w - x_off,
            height: (0.11 * win_h as f64) as i32,
        }
    }

    pub fn is_close_x_probe(
        &mut self,
        close_roi_bgr: Option<ArrayView3<'_, u8>>,
        frame_h: usize,
        threshold: f64,
    ) -> (bool, f64) {
        if frame_h < 500 {
            return (false, 0.0);
        }
        let Some(roi) = close_roi_bgr else {
            return (false, 0.0);
        };
        let (rh, rw, rc) = roi.dim();
        if rh < 4 || rw < 4 || rc < 3 {
            return (false, 0.0);
        }
        let scale_ui = frame_h as f64 / 1440.0;
        let (_, Some(tpl_close)) = self.get_scaled_anchors(scale_ui) else {
            return (false, 0.0);
        };
        let (th, tw, _) = tpl_close.dim();
        let roi_3 = if rc > 3 {
            roi.slice(s![.., .., 0..3])
        } else {
            roi
        };
        let Ok(ch) = resize_u8_hwc(
            roi_3,
            (rw / 2).max(tw),
            (rh / 2).max(th),
            Interpolation::Area,
        ) else {
            return (false, 0.0);
        };
        let Ok(res_c) =
            match_template_u8_hwc(ch.view(), tpl_close.view(), TemplateMatchMode::CcoeffNormed)
        else {
            return (false, 0.0);
        };
        let Ok((_, max_val, _, _)) = min_max_loc_f32(res_c.view()) else {
            return (false, 0.0);
        };
        let score = max_val as f64;
        (score >= threshold, score)
    }

    pub fn is_close_x_probe_frame(
        &mut self,
        close_roi: Option<FrameView<'_>>,
        frame_h: usize,
        threshold: f64,
    ) -> (bool, f64) {
        let Some(fv) = close_roi else {
            return (false, 0.0);
        };
        let Ok(bgr) = frame_view_to_bgr(fv) else {
            return (false, 0.0);
        };
        self.is_close_x_probe(Some(bgr.view()), frame_h, threshold)
    }

    pub fn compute_rgb_hist64(frame: ArrayView3<'_, u8>) -> [f32; 64] {
        rgb_hist64_sampled(frame)
    }

    pub fn bhattacharyya_distance(h1: &[f32; 64], h2: &[f32; 64]) -> f64 {
        ops_bhattacharyya_distance(h1, h2) as f64
    }

    fn compute_center_thumb(frame: ArrayView3<'_, u8>) -> Option<Array2<u8>> {
        let (h, w, _) = frame.dim();
        let y0 = (0.25 * h as f64) as usize;
        let y1 = (0.75 * h as f64) as usize;
        let x0 = (0.25 * w as f64) as usize;
        let x1 = (0.75 * w as f64) as usize;
        if y1 <= y0 || x1 <= x0 {
            return None;
        }
        let crop = frame.slice(s![y0..y1, x0..x1, ..]);
        let gray = bgr_to_gray(crop).ok()?;
        resize_u8(gray.view(), 64, 36, Interpolation::Area).ok()
    }

    fn compute_hud_thumb(frame: ArrayView3<'_, u8>) -> Option<Vec<u8>> {
        let (h, w, _) = frame.dim();
        let br_y0 = (0.86 * h as f64) as usize;
        let br_y1 = (0.98 * h as f64) as usize;
        let br_x0 = (0.75 * w as f64) as usize;
        let br_x1 = (0.98 * w as f64) as usize;

        let rt_y0 = (0.32 * h as f64) as usize;
        let rt_y1 = (0.68 * h as f64) as usize;
        let rt_x0 = (0.91 * w as f64) as usize;
        let rt_x1 = (0.99 * w as f64) as usize;

        if br_y1 <= br_y0 || br_x1 <= br_x0 || rt_y1 <= rt_y0 || rt_x1 <= rt_x0 {
            return None;
        }
        let br_g = bgr_to_gray(frame.slice(s![br_y0..br_y1, br_x0..br_x1, ..])).ok()?;
        let br = resize_u8(br_g.view(), 48, 16, Interpolation::Area).ok()?;
        let rt_g = bgr_to_gray(frame.slice(s![rt_y0..rt_y1, rt_x0..rt_x1, ..])).ok()?;
        let rt = resize_u8(rt_g.view(), 16, 48, Interpolation::Area).ok()?;

        let mut out = Vec::with_capacity(br.len() + rt.len());
        out.extend(br.iter().copied());
        out.extend(rt.iter().copied());
        Some(out)
    }

    pub fn is_map_m_open(&mut self, frame: Option<ArrayView3<'_, u8>>) -> MapOpenCheck {
        let Some(frame) = frame else {
            return MapOpenCheck::default();
        };
        let (h, w, c) = frame.dim();
        if h < 500 || w < 800 || c < 3 {
            return MapOpenCheck::default();
        }

        let slider_res = ZoomSliderDetector::detect_slider(frame);
        let match_score = slider_res.as_ref().map_or(0.0, |r| r.match_score);
        let slider_ok = slider_res.is_some() && match_score >= SLIDER_THRESHOLD;
        let slider_ccoeff = if slider_ok {
            slider_res.as_ref().map_or(0.0, |r| r.ccoeff)
        } else {
            0.0
        };
        let slider_strict_ok = slider_res.is_some()
            && match_score >= SLIDER_STRICT_MATCH
            && slider_ccoeff >= SLIDER_CCOEFF_THRESHOLD;

        let scale_ui = h as f64 / 1440.0;
        let (tpl_gear, _) = self.get_scaled_anchors(scale_ui);

        let mut gear_score = 0.0;
        if let Some(tg) = tpl_gear {
            let gy0 = (0.88 * h as f64) as usize;
            let gx1 = (0.09 * w as f64) as usize;
            if h > gy0 && gx1 > 0 {
                let gear_roi = frame.slice(s![gy0..h, 0..gx1, ..]);
                let (gh, gw, gc) = gear_roi.dim();
                if gh >= 4 && gw >= 4 && gc >= 3 {
                    let roi_3 = if gc > 3 {
                        gear_roi.slice(s![.., .., 0..3])
                    } else {
                        gear_roi
                    };
                    let (th, tw, _) = tg.dim();
                    if let Ok(resized) = resize_u8_hwc(
                        roi_3,
                        (gw / 2).max(tw),
                        (gh / 2).max(th),
                        Interpolation::Area,
                    ) && let Ok(res_g) = match_template_u8_hwc(
                        resized.view(),
                        tg.view(),
                        TemplateMatchMode::CcoeffNormed,
                    ) && let Ok((_, max_val, _, _)) = min_max_loc_f32(res_g.view())
                    {
                        gear_score = max_val as f64;
                    }
                }
            }
        }

        let cy1 = (0.11 * h as f64) as usize;
        let cx0 = (0.90 * w as f64) as usize;
        let close_score = if cy1 > 0 && w > cx0 {
            let close_roi = frame.slice(s![0..cy1, cx0..w, ..]);
            self.is_close_x_probe(Some(close_roi), h, ANCHOR_THRESHOLD).1
        } else {
            0.0
        };

        let gear_ok = gear_score >= ANCHOR_THRESHOLD;
        let close_ok = close_score >= ANCHOR_THRESHOLD;
        let anchor_count = u8::from(slider_ok) + u8::from(gear_ok) + u8::from(close_ok);
        let is_open = if self.has_anchor_templates() {
            slider_ok && gear_ok && close_ok
        } else {
            slider_strict_ok
        };

        MapOpenCheck {
            is_open,
            anchor_count,
            slider_strict_ok,
            slider_ccoeff,
            slider_res,
            gear_score,
            close_score,
        }
    }

    pub fn is_map_m_open_frame(&mut self, frame: Option<FrameView<'_>>) -> MapOpenCheck {
        let Some(fv) = frame else {
            return MapOpenCheck::default();
        };
        let Ok(bgr) = frame_view_to_bgr(fv) else {
            return MapOpenCheck::default();
        };
        self.is_map_m_open(Some(bgr.view()))
    }

    pub fn step(&mut self, frame: Option<ArrayView3<'_, u8>>) -> MapStepResult {
        let check = self.is_map_m_open(frame);
        let Some(frame) = frame else {
            self.reset();
            return MapStepResult {
                state: MapState::Closed,
                should_auto_sync: false,
                scene_cut: None,
                bhatt_dist: None,
                check,
            };
        };

        if !check.is_open {
            if self.state != MapState::Closed {
                // Drawer/popup hysteresis: keep open session alive on strict slider or Gear + 2 anchors.
                if check.slider_strict_ok
                    || (check.anchor_count == 2 && check.gear_score >= ANCHOR_THRESHOLD)
                {
                    self.closed_frames = 0;
                    if self.state == MapState::Settling {
                        self.state = MapState::Panning;
                    }
                    self.prev_thumb = Self::compute_center_thumb(frame);
                    self.prev_hud = Self::compute_hud_thumb(frame);
                    self.prev_hist64 = Some(Self::compute_rgb_hist64(frame));
                    return MapStepResult {
                        state: self.state,
                        should_auto_sync: false,
                        scene_cut: None,
                        bhatt_dist: None,
                        check,
                    };
                }

                // Instant scene-cut detection via 64-bin RGB Bhattacharyya distance.
                let mut bhatt = 0.0;
                if let Some(prev_h) = self.prev_hist64
                    && !frame.is_empty()
                {
                    let cur_h = Self::compute_rgb_hist64(frame);
                    bhatt = Self::bhattacharyya_distance(&prev_h, &cur_h);
                    if bhatt >= SCENE_CUT_THRESHOLD {
                        self.reset();
                        return MapStepResult {
                            state: MapState::Closed,
                            should_auto_sync: false,
                            scene_cut: Some(true),
                            bhatt_dist: Some(bhatt),
                            check,
                        };
                    }
                }

                self.closed_frames += 1;
                if self.closed_frames < CLOSE_DEBOUNCE_FRAMES {
                    return MapStepResult {
                        state: self.state,
                        should_auto_sync: false,
                        scene_cut: Some(false),
                        bhatt_dist: Some(bhatt),
                        check,
                    };
                }

                self.reset();
                return MapStepResult {
                    state: MapState::Closed,
                    should_auto_sync: false,
                    scene_cut: Some(false),
                    bhatt_dist: Some(bhatt),
                    check,
                };
            }

            self.reset();
            return MapStepResult {
                state: MapState::Closed,
                should_auto_sync: false,
                scene_cut: None,
                bhatt_dist: None,
                check,
            };
        }

        self.closed_frames = 0;
        let thumb = Self::compute_center_thumb(frame);
        let hud_thumb = Self::compute_hud_thumb(frame);
        self.prev_hist64 = Some(Self::compute_rgb_hist64(frame));

        if self.state == MapState::Closed {
            self.state = MapState::Settling;
            self.prev_thumb = thumb;
            self.prev_hud = hud_thumb;
            self.synced_thumb = None;
            self.stable_frames = 0;
            self.settling_frames = 0;
            self.settling_drag_frames = 0;
            self.retries = 0;
            return MapStepResult {
                state: self.state,
                should_auto_sync: false,
                scene_cut: None,
                bhatt_dist: None,
                check,
            };
        }

        let diff = match (&thumb, &self.prev_thumb) {
            (Some(t), Some(pt)) => mean_absdiff_2d(t, pt),
            _ => 0.0,
        };
        let hud_diff = match (&hud_thumb, &self.prev_hud) {
            (Some(ht), Some(pht)) => mean_absdiff_slice(ht, pht),
            _ => 0.0,
        };
        self.prev_thumb = thumb.clone();
        self.prev_hud = hud_thumb;

        if self.state == MapState::Settling {
            self.settling_frames += 1;
            if self.settling_frames >= 4 && diff > 8.0 && hud_diff < 2.5 {
                self.settling_drag_frames += 1;
            } else {
                self.settling_drag_frames = 0;
            }

            if self.settling_drag_frames >= 2 || self.settling_frames > MAX_SETTLING_FRAMES {
                self.state = MapState::Panning;
                return MapStepResult {
                    state: self.state,
                    should_auto_sync: false,
                    scene_cut: None,
                    bhatt_dist: None,
                    check,
                };
            }

            if diff < 3.5 && hud_diff < 3.5 {
                self.stable_frames += 1;
            } else {
                self.stable_frames = 0;
            }

            if self.stable_frames >= 2 {
                self.state = MapState::SyncedIdle;
                self.synced_thumb = thumb;
                return MapStepResult {
                    state: self.state,
                    should_auto_sync: true,
                    scene_cut: None,
                    bhatt_dist: None,
                    check,
                };
            }

            return MapStepResult {
                state: self.state,
                should_auto_sync: false,
                scene_cut: None,
                bhatt_dist: None,
                check,
            };
        }

        if diff > 4.5 {
            self.state = MapState::Panning;
        } else if diff < 3.0 && self.state == MapState::Panning {
            self.state = MapState::SyncedIdle;
        }

        MapStepResult {
            state: self.state,
            should_auto_sync: false,
            scene_cut: None,
            bhatt_dist: None,
            check,
        }
    }

    pub fn step_frame(&mut self, frame: Option<FrameView<'_>>) -> MapStepResult {
        let bgr = frame.and_then(|fv| frame_view_to_bgr(fv).ok());
        self.step(bgr.as_ref().map(|a| a.view()))
    }

    pub fn mark_synced(&mut self, frame: Option<ArrayView3<'_, u8>>) {
        if let Some(f) = frame {
            let (h, w, _) = f.dim();
            if h >= 500 && w >= 800 {
                let thumb = Self::compute_center_thumb(f);
                let hud = Self::compute_hud_thumb(f);
                self.prev_thumb = thumb.clone();
                self.prev_hud = hud;
                self.synced_thumb = thumb;
                self.prev_hist64 = Some(Self::compute_rgb_hist64(f));
            }
        }
        self.state = MapState::SyncedIdle;
        self.stable_frames = 2;
        self.closed_frames = 0;
        self.settling_frames = 0;
        self.settling_drag_frames = 0;
    }

    pub fn request_retry(&mut self) -> bool {
        if self.retries < MAX_RETRIES {
            self.retries += 1;
            self.state = MapState::Settling;
            self.stable_frames = 0;
            self.settling_frames = 0;
            self.settling_drag_frames = 0;
            return true;
        }
        self.state = MapState::SyncedIdle;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_io::{ImageReadMode, decode_image_bytes};

    const ZOOM_THUMB_PNG: &[u8] = include_bytes!("../../../../assets/zoom_thumb_template.png");

    fn synth_open_map_frame_no_anchors() -> Array3<u8> {
        let mut frame = Array3::<u8>::from_elem((1440, 2560, 3), 25u8);
        let tpl = decode_image_bytes(ZOOM_THUMB_PNG, ImageReadMode::Color)
            .unwrap()
            .into_bgr()
            .unwrap();
        let x0 = 48usize;
        let y0 = (1440.0 * 0.22) as usize + 80;
        for dy in 0..31 {
            for dx in 0..31 {
                for ch in 0..3 {
                    frame[[y0 + dy, x0 + dx, ch]] = tpl[[dy, dx, ch]];
                }
            }
        }
        // Subtle texture in center so center thumb has contrast
        for y in 400..1000 {
            for x in 700..1800 {
                frame[[y, x, 0]] = ((x + y) & 63) as u8;
                frame[[y, x, 1]] = ((x * 2) & 63) as u8;
            }
        }
        frame
    }

    #[test]
    fn probe_rects_and_bhattacharyya_sanity() {
        assert_eq!(
            MapScreenStateMachine::get_close_probe_rect(100, 50, 2560, 1440),
            RectI32 {
                x: 100 + 2304,
                y: 50,
                width: 256,
                height: 158
            }
        );
        assert_eq!(
            MapScreenStateMachine::get_slider_probe_rect(0, 0, 2560, 1440),
            RectI32 {
                x: 30,
                y: 316,
                width: 128,
                height: 663
            }
        );

        let dark = Array3::<u8>::from_elem((16, 16, 3), 10u8);
        let bright = Array3::<u8>::from_elem((16, 16, 3), 240u8);
        let h1 = MapScreenStateMachine::compute_rgb_hist64(dark.view());
        let h2 = MapScreenStateMachine::compute_rgb_hist64(bright.view());
        assert!((MapScreenStateMachine::bhattacharyya_distance(&h1, &h1)).abs() < 1e-6);
        assert!(MapScreenStateMachine::bhattacharyya_distance(&h1, &h2) > 0.99);
    }

    #[test]
    fn template_free_fallback_and_fsm_lifecycle() {
        let mut fsm = MapScreenStateMachine::with_anchors_path(None);
        assert!(!fsm.has_anchor_templates());

        let open_frame = synth_open_map_frame_no_anchors();
        let closed_frame = Array3::<u8>::from_elem((1440, 2560, 3), 220u8);

        // Flat bright frame rejected
        let r0 = fsm.step(Some(closed_frame.view()));
        assert_eq!(r0.state, MapState::Closed);
        assert!(!r0.should_auto_sync);

        // Open -> Settling -> 2 stable frames -> SyncedIdle + single auto-sync
        let r1 = fsm.step(Some(open_frame.view()));
        assert_eq!(r1.state, MapState::Settling);
        assert!(!r1.should_auto_sync);

        let r2 = fsm.step(Some(open_frame.view()));
        assert_eq!(r2.state, MapState::Settling);
        assert!(!r2.should_auto_sync);

        let r3 = fsm.step(Some(open_frame.view()));
        assert_eq!(r3.state, MapState::SyncedIdle);
        assert!(r3.should_auto_sync);

        // Never re-fires
        let r4 = fsm.step(Some(open_frame.view()));
        assert_eq!(r4.state, MapState::SyncedIdle);
        assert!(!r4.should_auto_sync);

        // Scene cut resets immediately in 1 frame
        let r5 = fsm.step(Some(closed_frame.view()));
        assert_eq!(r5.state, MapState::Closed);
        assert_eq!(r5.scene_cut, Some(true));
        assert!(r5.bhatt_dist.unwrap() >= SCENE_CUT_THRESHOLD);

        // Bounded retries
        assert!(fsm.request_retry());
        assert_eq!(fsm.retries(), 1);
        assert!(fsm.request_retry());
        assert_eq!(fsm.retries(), 2);
        assert!(!fsm.request_retry());
        assert_eq!(fsm.state(), MapState::SyncedIdle);
    }
}
