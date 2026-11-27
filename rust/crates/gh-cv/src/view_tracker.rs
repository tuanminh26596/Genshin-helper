//! EXPERIMENTAL — anchor + track for the v5.5 M-Map route guidance (measure before any UI).
//!
//! `MapLocator::locate_view` ("anchor") is accurate but slow (~90 ms, ~300 ms when it fails) and
//! a 3–4 waypoint match is wrong ~5 % of the time. Live guidance therefore runs two loops:
//! * every captured frame: a phase-correlation estimate of how the map content moved since the
//!   previous frame ([`ShiftEstimator`]: cursor-masked, zoom-aware via the slider), which carries
//!   the current screen→world transform forward ("track");
//! * on a worker, at its own pace: `locate_view`; a result for an older frame is carried forward
//!   through the recorded motions, then re-anchors the track when it agrees with it, or adds
//!   evidence to a candidate that is only shown once enough *independent* evidence agrees
//!   ([`ViewTracker`]).
//!
//! Measured on a 130 s real drag recording at 30 fps (`tests/e5_view_tracker_video.rs`); the
//! design decisions below cite what that recording showed.

use crate::locator::ViewRegistration;
use ndarray::ArrayView3;
use rayon::prelude::*;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;

/// Downscaled canvas (any input resolution is box-averaged to this).
/// Kept at 640×360 (512×256 FFT patch) to preserve 1440p/4K zoom & sub-pixel tracking accuracy
/// calibrated on the 130 s e5 recording while hitting < 4 ms/frame via zero-alloc banded FFT +
/// tiled transpose + cached horizontal bins.
pub const CANVAS_W: usize = 640;
pub const CANVAS_H: usize = 360;
/// Phase-correlation window inside the canvas; excludes the quest panel, zoom slider, top bar,
/// right-side buttons and the bottom-right region label.
pub const PATCH_X0: usize = 112;
pub const PATCH_Y0: usize = 52;
pub const PATCH_W: usize = 512;
pub const PATCH_H: usize = 256;
const PATCH_N: usize = PATCH_W * PATCH_H;
const TRANSPOSE_TILE: usize = 16;
/// Zoom ratios with `|ln r|` below this are treated as "no zoom" (slider jitter).
const ZOOM_EPS: f64 = 0.004;
/// The zoom-slider thumb jumps ahead of the map content, which follows ~1–2 frames later (on the
/// recording: slider 0.906 → 0.994 while the content went 0.978 → 0.906). The slider ratios are
/// therefore accumulated as a "zoom debt" and each frame tries these fractions of it, keeping the
/// strongest correlation.
const ZOOM_SEARCH_K: [f64; 4] = [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0];
/// Zoom debt not paid by the content within this many frames is dropped (slider misread).
/// Sized for 60 fps (~267 ms wall-clock horizon, matching 8 frames @ 30 fps).
const ZOOM_DEBT_MAX_FRAMES: u32 = 16;
/// Dedicated threads so the per-frame step never queues behind `locate_view` on rayon's pool.
const TRACKER_THREADS: usize = 4;

const _: () = assert!(PATCH_X0 + PATCH_W <= CANVAS_W);
const _: () = assert!(PATCH_Y0 + PATCH_H <= CANVAS_H);
const _: () = assert!((PATCH_W / TRANSPOSE_TILE) * TRANSPOSE_TILE == PATCH_W);
const _: () = assert!((PATCH_H / TRANSPOSE_TILE) * TRANSPOSE_TILE == PATCH_H);
const _: () = assert!((PATCH_W / TRACKER_THREADS) * TRACKER_THREADS == PATCH_W);
const _: () = assert!((PATCH_H / TRACKER_THREADS) * TRACKER_THREADS == PATCH_H);
/// Box around the mouse-cursor hotspot (px @1440p: x0, y0, x1, y1) hidden from phase correlation.
/// Genshin's cursor (white tip + yellow diamond, ~24 px) moves independently of the map — while
/// the map glides on after a flick, or while hovering — and over low-texture views such as the
/// sea it is the strongest feature left, so unmasked it hijacks the motion estimate (measured on
/// the 130 s drag recording: every gross motion error was the cursor's own movement).
const CURSOR_MASK_1440: [f64; 4] = [-18.0, -18.0, 46.0, 46.0];

/// Screen→world transform of the displayed M-Map view: `world = scale · screen + t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewTransform {
    pub map_id: i32,
    pub scale: f64,
    pub tx: f64,
    pub ty: f64,
}

impl ViewTransform {
    pub fn from_registration(r: &ViewRegistration) -> Self {
        Self {
            map_id: r.map_id,
            scale: r.scale,
            tx: r.tx,
            ty: r.ty,
        }
    }

    pub fn screen_to_world(&self, x: f64, y: f64) -> (f64, f64) {
        (self.scale * x + self.tx, self.scale * y + self.ty)
    }

    pub fn world_to_screen(&self, wx: f64, wy: f64) -> (f64, f64) {
        let s = if self.scale.abs() < 1e-12 { 1e-12 } else { self.scale };
        ((wx - self.tx) / s, (wy - self.ty) / s)
    }

    /// Carries the transform from frame A to frame B where content moved `p_B = r·p_A + e`.
    pub fn apply_motion(&self, m: &ViewMotion) -> Self {
        let r = if m.r.abs() < 1e-9 { 1.0 } else { m.r };
        Self {
            map_id: self.map_id,
            scale: self.scale / r,
            tx: self.tx - self.scale * m.ex / r,
            ty: self.ty - self.scale * m.ey / r,
        }
    }

    /// Max screen-space disagreement (px, normalised to 1440p) over a 3×3 grid covering the
    /// central 80 % of a `w × h` frame. Different maps → `f64::INFINITY`.
    pub fn disagreement_px_1440(&self, other: &Self, w: usize, h: usize) -> f64 {
        if self.map_id != other.map_id {
            return f64::INFINITY;
        }
        let mut worst = 0.0f64;
        for fy in [0.1, 0.5, 0.9] {
            for fx in [0.1, 0.5, 0.9] {
                let (x, y) = (fx * w as f64, fy * h as f64);
                let (wx, wy) = self.screen_to_world(x, y);
                let (ox, oy) = other.world_to_screen(wx, wy);
                worst = worst.max((ox - x).hypot(oy - y));
            }
        }
        worst * 1440.0 / h.max(1) as f64
    }
}

/// Frame-to-frame motion of map content in full-resolution px: `p_B = r · p_A + e`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewMotion {
    pub r: f64,
    pub ex: f64,
    pub ey: f64,
    /// Phase-correlation peak (0..1); higher = more reliable.
    pub peak: f32,
}

struct FftPlans {
    fwd_row: Arc<dyn Fft<f32>>,
    fwd_col: Arc<dyn Fft<f32>>,
    inv_row: Arc<dyn Fft<f32>>,
    inv_col: Arc<dyn Fft<f32>>,
}

/// Zoom-aware, cursor-masked phase-correlation motion estimator between consecutive frames.
pub struct ShiftEstimator {
    pool: Arc<rayon::ThreadPool>,
    plans: FftPlans,
    window: Vec<f32>,
    canvas: Vec<f32>,
    prev_canvas: Vec<f32>,
    /// Cached horizontal box-downsample bin ranges `[x0, x1)` for `frame_dims.0`.
    x_bins: [(usize, usize); CANVAS_W],
    prev_fft: Vec<Complex<f32>>,
    ref_fft: Vec<Complex<f32>>,
    cur_fft: Vec<Complex<f32>>,
    work: Vec<Complex<f32>>,
    tbuf: Vec<Complex<f32>>,
    /// Reusable per-band FFT scratch (`TRACKER_THREADS * fft_scratch_stride` elements).
    fft_scratch: Vec<Complex<f32>>,
    fft_scratch_stride: usize,
    has_prev: bool,
    frame_dims: (usize, usize),
    zoom_debt: f64,
    zoom_debt_age: u32,
}

impl Default for ShiftEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl ShiftEstimator {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let plans = FftPlans {
            fwd_row: planner.plan_fft_forward(PATCH_W),
            fwd_col: planner.plan_fft_forward(PATCH_H),
            inv_row: planner.plan_fft_inverse(PATCH_W),
            inv_col: planner.plan_fft_inverse(PATCH_H),
        };
        let fft_scratch_stride = plans
            .fwd_row
            .get_inplace_scratch_len()
            .max(plans.fwd_col.get_inplace_scratch_len())
            .max(plans.inv_row.get_inplace_scratch_len())
            .max(plans.inv_col.get_inplace_scratch_len())
            .max(1);
        let mut window = vec![0f32; PATCH_N];
        for y in 0..PATCH_H {
            let wy = 0.5 - 0.5 * (std::f32::consts::TAU * y as f32 / (PATCH_H - 1) as f32).cos();
            for x in 0..PATCH_W {
                let wx = 0.5 - 0.5 * (std::f32::consts::TAU * x as f32 / (PATCH_W - 1) as f32).cos();
                window[y * PATCH_W + x] = wx * wy;
            }
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(TRACKER_THREADS)
            .thread_name(|i| format!("gh-view-track-{i}"))
            .build()
            .unwrap_or_else(|_| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .expect("single-thread rayon pool")
            });
        let zero = Complex::new(0f32, 0f32);
        Self {
            pool: Arc::new(pool),
            plans,
            window,
            canvas: vec![0f32; CANVAS_W * CANVAS_H],
            prev_canvas: vec![0f32; CANVAS_W * CANVAS_H],
            x_bins: [(0, 1); CANVAS_W],
            prev_fft: vec![zero; PATCH_N],
            ref_fft: vec![zero; PATCH_N],
            cur_fft: vec![zero; PATCH_N],
            work: vec![zero; PATCH_N],
            tbuf: vec![zero; PATCH_N],
            fft_scratch: vec![zero; TRACKER_THREADS * fft_scratch_stride],
            fft_scratch_stride,
            has_prev: false,
            frame_dims: (0, 0),
            zoom_debt: 1.0,
            zoom_debt_age: 0,
        }
    }

    /// Forgets the previous frame (next call returns `None`).
    pub fn reset(&mut self) {
        self.has_prev = false;
        self.zoom_debt = 1.0;
        self.zoom_debt_age = 0;
    }

    /// Ingests a frame; returns the motion since the previous frame. `zoom_r` = magnification the
    /// zoom slider reports since the previous frame (`prev_scale / cur_scale`); the content may
    /// follow it a frame or two late (see [`ZOOM_SEARCH_K`]). `cursor` = mouse-cursor hotspot in
    /// frame px (masked out, see [`CURSOR_MASK_1440`]).
    pub fn push_frame(
        &mut self,
        frame_bgr: ArrayView3<'_, u8>,
        zoom_r: Option<f64>,
        cursor: Option<(f64, f64)>,
    ) -> Option<ViewMotion> {
        let pool = Arc::clone(&self.pool);
        pool.install(|| self.push_frame_inner(frame_bgr, zoom_r, cursor))
    }

    fn push_frame_inner(
        &mut self,
        frame_bgr: ArrayView3<'_, u8>,
        zoom_r: Option<f64>,
        cursor: Option<(f64, f64)>,
    ) -> Option<ViewMotion> {
        let (h, w, c) = frame_bgr.dim();
        if h < CANVAS_H || w < CANVAS_W || c < 3 {
            self.reset();
            return None;
        }
        if self.frame_dims != (w, h) {
            self.reset();
            self.frame_dims = (w, h);
            for (i, slot) in self.x_bins.iter_mut().enumerate() {
                let x0 = i * w / CANVAS_W;
                *slot = (x0, ((i + 1) * w / CANVAS_W).max(x0 + 1));
            }
        }
        build_canvas(frame_bgr, &self.x_bins, &mut self.canvas);
        if let Some(c) = cursor {
            mask_cursor(&mut self.canvas, w, h, c);
        }
        fill_patch(&self.canvas, &self.window, &mut self.cur_fft, None);
        fft2(
            &mut self.cur_fft,
            &mut self.tbuf,
            &mut self.fft_scratch,
            self.fft_scratch_stride,
            &self.plans.fwd_row,
            &self.plans.fwd_col,
        );

        let motion = if self.has_prev {
            let rs = zoom_r.filter(|r| r.is_finite() && *r > 0.2 && *r < 5.0).unwrap_or(1.0);
            if rs.ln().abs() >= ZOOM_EPS {
                self.zoom_debt *= rs;
            }
            let debt = self.zoom_debt;
            let ks: &[f64] = if debt.ln().abs() < ZOOM_EPS { &[0.0] } else { &ZOOM_SEARCH_K };
            let mut best: Option<ViewMotion> = None;
            for &k in ks {
                let r = if k == 0.0 { 1.0 } else { debt.powf(k) };
                let m = self.correlate(r, w, h);
                if best.is_none_or(|b| m.peak > b.peak) {
                    best = Some(m);
                }
            }
            if let Some(b) = best {
                self.zoom_debt /= b.r;
                if self.zoom_debt.ln().abs() < ZOOM_EPS {
                    self.zoom_debt = 1.0;
                    self.zoom_debt_age = 0;
                } else {
                    self.zoom_debt_age += 1;
                    if self.zoom_debt_age > ZOOM_DEBT_MAX_FRAMES {
                        self.zoom_debt = 1.0;
                        self.zoom_debt_age = 0;
                    }
                }
            }
            best
        } else {
            self.zoom_debt = 1.0;
            self.zoom_debt_age = 0;
            None
        };
        std::mem::swap(&mut self.canvas, &mut self.prev_canvas);
        std::mem::swap(&mut self.prev_fft, &mut self.cur_fft);
        self.has_prev = true;
        motion
    }

    /// Phase correlation of the current patch against the previous one magnified by `r`.
    fn correlate(&mut self, r: f64, w: usize, h: usize) -> ViewMotion {
        if r != 1.0 {
            fill_patch(&self.prev_canvas, &self.window, &mut self.ref_fft, Some(r));
            fft2(
                &mut self.ref_fft,
                &mut self.tbuf,
                &mut self.fft_scratch,
                self.fft_scratch_stride,
                &self.plans.fwd_row,
                &self.plans.fwd_col,
            );
        }
        let reference = if r != 1.0 { &self.ref_fft } else { &self.prev_fft };
        const CROSS_CHUNK: usize = PATCH_W * 8;
        self.work
            .par_chunks_mut(CROSS_CHUNK)
            .zip(reference.par_chunks(CROSS_CHUNK))
            .zip(self.cur_fft.par_chunks(CROSS_CHUNK))
            .for_each(|((out, a), b)| {
                for ((o, &av), &bv) in out.iter_mut().zip(a).zip(b) {
                    let p = av.conj() * bv;
                    let nsq = p.norm_sqr();
                    *o = if nsq > 1e-24 && nsq.is_finite() {
                        let inv = nsq.sqrt().recip();
                        Complex::new(p.re * inv, p.im * inv)
                    } else {
                        let m = p.norm();
                        if m > 1e-12 { p / m } else { Complex::new(0.0, 0.0) }
                    };
                }
            });
        fft2(
            &mut self.work,
            &mut self.tbuf,
            &mut self.fft_scratch,
            self.fft_scratch_stride,
            &self.plans.inv_row,
            &self.plans.inv_col,
        );
        let (dx, dy, peak) = find_peak(&self.work);
        let (fx, fy) = (w as f64 / CANVAS_W as f64, h as f64 / CANVAS_H as f64);
        let (cx, cy) = (
            PATCH_X0 as f64 + (PATCH_W as f64 - 1.0) / 2.0,
            PATCH_Y0 as f64 + (PATCH_H as f64 - 1.0) / 2.0,
        );
        // Canvas pixel `u` averages frame pixels `[u*fx, (u+1)*fx)`, centered at `(u + 0.5)*fx - 0.5`.
        // Mapping `p_B = r * p_A + e` through those bin centers eliminates downscale zoom bias.
        ViewMotion {
            r,
            ex: fx * ((1.0 - r) * (cx + 0.5) + dx) - 0.5 * (1.0 - r),
            ey: fy * ((1.0 - r) * (cy + 0.5) + dy) - 0.5 * (1.0 - r),
            peak,
        }
    }
}

/// Box-averages a BGR/BGRA frame into the `CANVAS_W × CANVAS_H` luma canvas (parallel bands).
fn build_canvas(frame: ArrayView3<'_, u8>, xb: &[(usize, usize); CANVAS_W], out: &mut [f32]) {
    let (h, _w, c) = frame.dim();
    const BAND_ROWS: usize = 30; // 360 / 30 = 12 balanced tasks across TRACKER_THREADS
    out.par_chunks_mut(CANVAS_W * BAND_ROWS)
        .enumerate()
        .for_each(|(band_idx, band_out)| {
            let mut acc = [0u32; CANVAS_W];
            for (local_y, out_row) in band_out.as_chunks_mut::<CANVAS_W>().0.iter_mut().enumerate()
            {
                let oy = band_idx * BAND_ROWS + local_y;
                let y0 = oy * h / CANVAS_H;
                let y1 = ((oy + 1) * h / CANVAS_H).max(y0 + 1);
                acc.fill(0);
                for y in y0..y1 {
                    let row = frame.slice(ndarray::s![y, .., ..]);
                    if let Some(px) = row.as_slice() {
                        match c {
                            3 => {
                                for (ox, &(x0, x1)) in xb.iter().enumerate() {
                                    let mut s = 0u32;
                                    for p in px[x0 * 3..x1 * 3].as_chunks::<3>().0 {
                                        s += 29 * u32::from(p[0])
                                            + 150 * u32::from(p[1])
                                            + 77 * u32::from(p[2]);
                                    }
                                    acc[ox] += s;
                                }
                            }
                            4 => {
                                for (ox, &(x0, x1)) in xb.iter().enumerate() {
                                    let mut s = 0u32;
                                    for p in px[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                                        s += 29 * u32::from(p[0])
                                            + 150 * u32::from(p[1])
                                            + 77 * u32::from(p[2]);
                                    }
                                    acc[ox] += s;
                                }
                            }
                            _ => {
                                for (ox, &(x0, x1)) in xb.iter().enumerate() {
                                    let mut s = 0u32;
                                    for p in px[x0 * c..x1 * c].chunks_exact(c) {
                                        s += 29 * u32::from(p[0])
                                            + 150 * u32::from(p[1])
                                            + 77 * u32::from(p[2]);
                                    }
                                    acc[ox] += s;
                                }
                            }
                        }
                    } else {
                        for (ox, &(x0, x1)) in xb.iter().enumerate() {
                            let mut s = 0u32;
                            for x in x0..x1 {
                                s += 29 * u32::from(row[[x, 0]])
                                    + 150 * u32::from(row[[x, 1]])
                                    + 77 * u32::from(row[[x, 2]]);
                            }
                            acc[ox] += s;
                        }
                    }
                }
                let dy = (y1 - y0) as f32 * 256.0;
                for (ox, &(x0, x1)) in xb.iter().enumerate() {
                    let n = (x1 - x0) as f32 * dy;
                    out_row[ox] = acc[ox] as f32 / n;
                }
            }
        });
}

/// Flattens the cursor box on the canvas to the mean of its 1-px outer border, so neither the
/// cursor nor a hard-edged hole remains for the correlation to lock onto. The masked canvas is
/// also the next frame's reference, so the previous frame's cursor stays hidden too.
fn mask_cursor(canvas: &mut [f32], frame_w: usize, frame_h: usize, cursor: (f64, f64)) {
    let k = frame_h as f64 / 1440.0;
    let (sx, sy) = (CANVAS_W as f64 / frame_w.max(1) as f64, CANVAS_H as f64 / frame_h.max(1) as f64);
    let [bx0, by0, bx1, by1] = CURSOR_MASK_1440;
    let x0 = ((cursor.0 + bx0 * k) * sx).floor();
    let y0 = ((cursor.1 + by0 * k) * sy).floor();
    let x1 = ((cursor.0 + bx1 * k) * sx).ceil();
    let y1 = ((cursor.1 + by1 * k) * sy).ceil();
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return;
    }
    let cx = |v: f64| v.clamp(0.0, CANVAS_W as f64) as usize;
    let cy = |v: f64| v.clamp(0.0, CANVAS_H as f64) as usize;
    let (x0, x1, y0, y1) = (cx(x0), cx(x1), cy(y0), cy(y1));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (ox0, ox1) = (x0.saturating_sub(1), (x1 + 1).min(CANVAS_W));
    let (oy0, oy1) = (y0.saturating_sub(1), (y1 + 1).min(CANVAS_H));
    let (mut sum, mut n) = (0f64, 0usize);
    for y in oy0..oy1 {
        for x in ox0..ox1 {
            if y < y0 || y >= y1 || x < x0 || x >= x1 {
                sum += f64::from(canvas[y * CANVAS_W + x]);
                n += 1;
            }
        }
    }
    if n == 0 {
        return;
    }
    let fill = (sum / n as f64) as f32;
    for y in y0..y1 {
        canvas[y * CANVAS_W + x0..y * CANVAS_W + x1].fill(fill);
    }
}

/// Windowed, mean-removed patch; with `Some(r)` the canvas is magnified by `r` about the patch
/// centre (bilinear) so that a zoomed previous frame lines up with the current one.
fn fill_patch(canvas: &[f32], window: &[f32], out: &mut [Complex<f32>], magnify: Option<f64>) {
    let sum: f64 = match magnify {
        None => {
            let mut acc = 0f64;
            for py in 0..PATCH_H {
                let src = &canvas[(PATCH_Y0 + py) * CANVAS_W + PATCH_X0
                    ..(PATCH_Y0 + py) * CANVAS_W + PATCH_X0 + PATCH_W];
                let dst = &mut out[py * PATCH_W..(py + 1) * PATCH_W];
                for (o, &v) in dst.iter_mut().zip(src) {
                    acc += f64::from(v);
                    *o = Complex::new(v, 0.0);
                }
            }
            acc
        }
        Some(r) => {
            let cx = PATCH_X0 as f64 + (PATCH_W as f64 - 1.0) / 2.0;
            let cy = PATCH_Y0 as f64 + (PATCH_H as f64 - 1.0) / 2.0;
            let inv_r = 1.0 / r;
            let max_x = (CANVAS_W - 1) as f64;
            let max_y = (CANVAS_H - 1) as f64;
            let row_sums: Vec<f64> = out
                .par_chunks_mut(PATCH_W)
                .enumerate()
                .map(|(py, dst)| {
                    let sy = (cy + ((PATCH_Y0 + py) as f64 - cy) * inv_r).clamp(0.0, max_y);
                    let y0 = sy.floor() as usize;
                    let y1 = (y0 + 1).min(CANVAS_H - 1);
                    let ay = (sy - y0 as f64) as f32;
                    let one_m_ay = 1.0 - ay;
                    let row0 = &canvas[y0 * CANVAS_W..(y0 + 1) * CANVAS_W];
                    let row1 = &canvas[y1 * CANVAS_W..(y1 + 1) * CANVAS_W];
                    let mut row_sum = 0f64;
                    for (px, o) in dst.iter_mut().enumerate() {
                        let sx = (cx + ((PATCH_X0 + px) as f64 - cx) * inv_r).clamp(0.0, max_x);
                        let x0 = sx.floor() as usize;
                        let x1 = (x0 + 1).min(CANVAS_W - 1);
                        let ax = (sx - x0 as f64) as f32;
                        let top = row0[x0] + (row0[x1] - row0[x0]) * ax;
                        let bot = row1[x0] + (row1[x1] - row1[x0]) * ax;
                        let v = top * one_m_ay + bot * ay;
                        row_sum += f64::from(v);
                        *o = Complex::new(v, 0.0);
                    }
                    row_sum
                })
                .collect();
            row_sums.into_iter().sum()
        }
    };
    let mean = (sum / PATCH_N as f64) as f32;
    for (o, &wv) in out.iter_mut().zip(window) {
        o.re = (o.re - mean) * wv;
    }
}

/// Rows of `fft.len()` complex values processed in `TRACKER_THREADS` parallel bands using
/// preallocated scratch slices (zero per-frame heap allocation).
fn fft_rows(
    data: &mut [Complex<f32>],
    scratch: &mut [Complex<f32>],
    scratch_stride: usize,
    fft: &Arc<dyn Fft<f32>>,
) {
    let band = data.len() / TRACKER_THREADS;
    debug_assert_eq!(band * TRACKER_THREADS, data.len());
    debug_assert!(band.is_multiple_of(fft.len()));
    data.par_chunks_mut(band)
        .zip(scratch.par_chunks_mut(scratch_stride))
        .for_each(|(block, sc)| fft.process_with_scratch(block, sc));
}

/// 2-D FFT of a `PATCH_H × PATCH_W` buffer: rows, tiled transpose, rows (= columns), transpose back.
fn fft2(
    data: &mut [Complex<f32>],
    tbuf: &mut [Complex<f32>],
    scratch: &mut [Complex<f32>],
    scratch_stride: usize,
    row: &Arc<dyn Fft<f32>>,
    col: &Arc<dyn Fft<f32>>,
) {
    fft_rows(data, scratch, scratch_stride, row);
    {
        let src: &[Complex<f32>] = data;
        tbuf.par_chunks_mut(TRANSPOSE_TILE * PATCH_H)
            .enumerate()
            .for_each(|(tc, dst_band)| {
                let c0 = tc * TRANSPOSE_TILE;
                let mut blk = [Complex::new(0f32, 0f32); TRANSPOSE_TILE * TRANSPOSE_TILE];
                for r0 in (0..PATCH_H).step_by(TRANSPOSE_TILE) {
                    for dr in 0..TRANSPOSE_TILE {
                        let src_row = &src[(r0 + dr) * PATCH_W + c0
                            ..(r0 + dr) * PATCH_W + c0 + TRANSPOSE_TILE];
                        for (dc, &val) in src_row.iter().enumerate() {
                            blk[dc * TRANSPOSE_TILE + dr] = val;
                        }
                    }
                    for dc in 0..TRANSPOSE_TILE {
                        dst_band[dc * PATCH_H + r0..dc * PATCH_H + r0 + TRANSPOSE_TILE]
                            .copy_from_slice(
                                &blk[dc * TRANSPOSE_TILE..(dc + 1) * TRANSPOSE_TILE],
                            );
                    }
                }
            });
    }
    fft_rows(tbuf, scratch, scratch_stride, col);
    let src: &[Complex<f32>] = tbuf;
    data.par_chunks_mut(TRANSPOSE_TILE * PATCH_W)
        .enumerate()
        .for_each(|(tr, dst_band)| {
            let r0 = tr * TRANSPOSE_TILE;
            let mut blk = [Complex::new(0f32, 0f32); TRANSPOSE_TILE * TRANSPOSE_TILE];
            for c0 in (0..PATCH_W).step_by(TRANSPOSE_TILE) {
                for dc in 0..TRANSPOSE_TILE {
                    let src_row = &src[(c0 + dc) * PATCH_H + r0
                        ..(c0 + dc) * PATCH_H + r0 + TRANSPOSE_TILE];
                    for (dr, &val) in src_row.iter().enumerate() {
                        blk[dr * TRANSPOSE_TILE + dc] = val;
                    }
                }
                for dr in 0..TRANSPOSE_TILE {
                    dst_band[dr * PATCH_W + c0..dr * PATCH_W + c0 + TRANSPOSE_TILE]
                        .copy_from_slice(&blk[dr * TRANSPOSE_TILE..(dr + 1) * TRANSPOSE_TILE]);
                }
            }
        });
}

/// Peak of the correlation surface → sub-pixel content displacement (canvas px) and strength.
fn find_peak(surface: &[Complex<f32>]) -> (f64, f64, f32) {
    let n = PATCH_N as f32;
    let (mut best, mut bi) = (f32::MIN, 0usize);
    for (i, v) in surface.iter().enumerate() {
        if v.re > best {
            best = v.re;
            bi = i;
        }
    }
    let (px, py) = (bi % PATCH_W, bi / PATCH_W);
    let at = |x: isize, y: isize| {
        let xx = x.rem_euclid(PATCH_W as isize) as usize;
        let yy = y.rem_euclid(PATCH_H as isize) as usize;
        f64::from(surface[yy * PATCH_W + xx].re)
    };
    let sub = |m: f64, c: f64, p: f64| {
        let d = m - 2.0 * c + p;
        if d.abs() < 1e-12 { 0.0 } else { (0.5 * (m - p) / d).clamp(-0.5, 0.5) }
    };
    let (ix, iy) = (px as isize, py as isize);
    let c0 = at(ix, iy);
    let ox = sub(at(ix - 1, iy), c0, at(ix + 1, iy));
    let oy = sub(at(ix, iy - 1), c0, at(ix, iy + 1));
    let mut dx = px as f64 + ox;
    let mut dy = py as f64 + oy;
    if dx > (PATCH_W / 2) as f64 {
        dx -= PATCH_W as f64;
    }
    if dy > (PATCH_H / 2) as f64 {
        dy -= PATCH_H as f64;
    }
    (dx, dy, best / n)
}

/// Tunables for [`ViewTracker`] (defaults chosen on the 130 s e5 recording).
#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    /// Motions with at least this phase peak always carry the transform.
    pub min_peak: f32,
    /// Motions between `weak_peak` and `min_peak` carry it only when they continue the previous
    /// frame's velocity (drags and post-flick glides are smooth at 30 fps, a spurious peak is not);
    /// otherwise the transform is hidden until a registration confirms it. Below: lost.
    pub weak_peak: f32,
    pub velocity_abs_px_1440: f64,
    pub velocity_rel: f64,
    /// A registration within this of the tracked transform re-anchors it.
    pub agree_px_1440: f64,
    /// Registration evidence points (see [`ViewTracker::evidence_points`]).
    pub strong_inliers: usize,
    pub strong_max_rmse: f64,
    pub four_point_min_terrain: f64,
    /// Points a candidate needs to be shown when nothing is shown / to replace what is shown.
    pub acquire_points: u32,
    pub switch_points: u32,
    /// Registrations with fewer than `same_view_min_inliers` waypoints only add up once the view
    /// moved / zoomed this much in between: near-identical frames repeat the same false
    /// constellation match (seen on the recording during a map-entry animation, where the zoom
    /// slider misreported the scale for several static frames).
    pub distinct_view_px_1440: f64,
    pub distinct_view_zoom: f64,
    pub same_view_min_inliers: usize,
    /// Drop the track after this many frames without an agreeing registration.
    pub max_track_frames: u32,
    /// A hidden transform is forgotten after this many frames.
    pub hidden_max_frames: u32,
    /// Motions kept to carry late (worker) registrations forward.
    pub history_len: usize,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            min_peak: 0.08,
            weak_peak: 0.04,
            velocity_abs_px_1440: 30.0,
            velocity_rel: 0.4,
            agree_px_1440: 40.0,
            strong_inliers: 8,
            strong_max_rmse: 3.5,
            four_point_min_terrain: 0.15,
            acquire_points: 4,
            switch_points: 6,
            distinct_view_px_1440: 48.0,
            distinct_view_zoom: 0.03,
            same_view_min_inliers: 5,
            max_track_frames: 600,
            hidden_max_frames: 90,
            history_len: 240,
        }
    }
}

/// Zoom-slider reading for one frame: world units per px from
/// `ZoomSliderDetector::get_map_scale` and the map whose formula produced it. Only ratios between
/// consecutive readings of the same map are used (switching maps changes the formula).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SliderScale {
    pub map_id: i32,
    pub scale: f64,
}

/// Where the current transform comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSource {
    /// A registration was accepted on this frame.
    Anchored,
    /// Carried forward from an earlier anchor by frame-to-frame motion.
    Tracked,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackerOutput {
    pub transform: Option<ViewTransform>,
    pub source: Option<TrackSource>,
    pub frames_since_anchor: u32,
}

/// What a registration did — for measurement and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationVerdict {
    /// Agreed with the shown transform (re-anchored it).
    Agrees,
    /// Agreed with the hidden transform, which is shown again.
    Restored,
    /// Candidate reached `acquire_points` → shown.
    Acquired,
    /// Candidate reached `switch_points` → replaced the shown transform.
    Switched,
    /// Started a new candidate (nothing shown changes).
    CandidateStarted,
    /// Added evidence to the candidate (nothing shown changes yet).
    CandidateReinforced,
    /// Could not be carried to the current frame (a lost motion in between).
    Stale,
    NotConfident,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionClass {
    Reliable,
    Weak,
    Lost,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    t: ViewTransform,
    points: u32,
    /// View change since the candidate last gained distinct-view evidence.
    moved_px_1440: f64,
    zoom: f64,
}

pub struct ViewTracker {
    cfg: TrackerConfig,
    est: ShiftEstimator,
    seq: u64,
    frame_dims: (usize, usize),
    last_slider: Option<SliderScale>,
    /// Motion that produced frame `seq` from `seq - 1`.
    history: VecDeque<(u64, Option<ViewMotion>, MotionClass)>,
    state: Option<ViewTransform>,
    anchored_this_frame: bool,
    frames_since_anchor: u32,
    /// Shown transform hidden after a weak motion; one agreeing registration restores it.
    hidden: Option<(ViewTransform, u32)>,
    candidate: Option<Candidate>,
    /// Last carried motion (px @1440p) for the velocity-continuity test (`None` across zooms).
    last_velocity: Option<(f64, f64)>,
}

impl Default for ViewTracker {
    fn default() -> Self {
        Self::new(TrackerConfig::default())
    }
}

impl ViewTracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self {
            cfg,
            est: ShiftEstimator::new(),
            seq: 0,
            frame_dims: (0, 0),
            last_slider: None,
            history: VecDeque::new(),
            state: None,
            anchored_this_frame: false,
            frames_since_anchor: 0,
            hidden: None,
            candidate: None,
            last_velocity: None,
        }
    }

    pub fn current_seq(&self) -> u64 {
        self.seq
    }

    /// Whether the latest frame continued the previous one (motion not lost). The registration
    /// worker should only start on such frames: on the recording, a registration started during a
    /// map-switch transition kept the single worker busy right when the new map appeared
    /// (Ancient Sacred Mountain entry: 89 % → 96 % coverage with this rule).
    pub fn frame_is_stable(&self) -> bool {
        self.history.back().is_some_and(|(_, _, c)| *c != MotionClass::Lost)
    }

    /// Clears everything (M-Map closed, game window lost, ...).
    pub fn reset(&mut self) {
        self.est.reset();
        self.history.clear();
        self.state = None;
        self.hidden = None;
        self.candidate = None;
        self.last_slider = None;
        self.last_velocity = None;
        self.frames_since_anchor = 0;
    }

    /// Feeds one captured frame. `slider` = this frame's zoom-slider reading (see
    /// [`SliderScale`]); `cursor` = mouse hotspot in frame px (`GetCursorPos` mapped into the
    /// capture), masked out of the motion estimate. Returns the new frame's sequence id.
    pub fn on_frame(
        &mut self,
        frame_bgr: ArrayView3<'_, u8>,
        slider: Option<SliderScale>,
        cursor: Option<(f64, f64)>,
    ) -> u64 {
        let (h, w, _) = frame_bgr.dim();
        let zoom_r = slider_zoom_ratio(self.last_slider, slider);
        if slider.is_some() {
            self.last_slider = slider;
        }
        let motion = self.est.push_frame(frame_bgr, zoom_r, cursor);
        self.on_motion(motion, w, h)
    }

    fn classify(&self, m: Option<&ViewMotion>) -> MotionClass {
        let Some(m) = m else { return MotionClass::Lost };
        if m.peak < self.cfg.weak_peak {
            return MotionClass::Lost;
        }
        if m.peak >= self.cfg.min_peak {
            return MotionClass::Reliable;
        }
        if let Some((px, py)) = self.last_velocity
            && m.r == 1.0
        {
            let k = 1440.0 / self.frame_dims.1.max(1) as f64;
            let (ex, ey) = (m.ex * k, m.ey * k);
            let tol = self.cfg.velocity_abs_px_1440.max(self.cfg.velocity_rel * px.hypot(py));
            if (ex - px).hypot(ey - py) <= tol {
                return MotionClass::Reliable;
            }
        }
        MotionClass::Weak
    }

    /// Advances one frame with an externally estimated motion (`None` = no estimate). Used by
    /// [`Self::on_frame`] and by offline replays of recorded motions.
    pub fn on_motion(&mut self, motion: Option<ViewMotion>, w: usize, h: usize) -> u64 {
        self.frame_dims = (w, h);
        let class = self.classify(motion.as_ref());
        self.seq += 1;
        self.history.push_back((self.seq, motion, class));
        while self.history.len() > self.cfg.history_len {
            self.history.pop_front();
        }
        self.anchored_this_frame = false;
        let m = match (class, motion) {
            (MotionClass::Lost, _) | (_, None) => {
                self.state = None;
                self.hidden = None;
                self.candidate = None;
                self.last_velocity = None;
                return self.seq;
            }
            (_, Some(m)) => m,
        };
        let k = 1440.0 / h.max(1) as f64;
        self.last_velocity = (m.r == 1.0).then_some((m.ex * k, m.ey * k));
        if let Some(c) = self.candidate.as_mut() {
            c.t = c.t.apply_motion(&m);
            c.moved_px_1440 += m.ex.hypot(m.ey) * k;
            c.zoom *= m.r;
        }
        if let Some((t, age)) = self.hidden {
            self.hidden = (age < self.cfg.hidden_max_frames).then(|| (t.apply_motion(&m), age + 1));
        }
        if let Some(s) = self.state {
            let carried = s.apply_motion(&m);
            self.frames_since_anchor = self.frames_since_anchor.saturating_add(1);
            if class == MotionClass::Weak {
                self.hidden = Some((carried, 0));
                self.state = None;
            } else if self.frames_since_anchor > self.cfg.max_track_frames {
                self.state = None;
            } else {
                self.state = Some(carried);
            }
        }
        self.seq
    }

    /// Carries a transform registered on frame `frame_seq` to the current frame. Returns it and
    /// whether every motion on the way was reliable.
    fn carry_forward(&self, t: ViewTransform, frame_seq: u64) -> Option<(ViewTransform, bool)> {
        if frame_seq > self.seq {
            return None;
        }
        if frame_seq == self.seq {
            return Some((t, true));
        }
        let oldest = self.history.front().map(|(s, _, _)| *s)?;
        if frame_seq + 1 < oldest {
            return None;
        }
        let (mut out, mut reliable) = (t, true);
        for (s, m, class) in &self.history {
            if *s <= frame_seq {
                continue;
            }
            match (class, m) {
                (MotionClass::Lost, _) | (_, None) => return None,
                (MotionClass::Weak, Some(m)) => {
                    reliable = false;
                    out = out.apply_motion(m);
                }
                (MotionClass::Reliable, Some(m)) => out = out.apply_motion(m),
            }
        }
        Some((out, reliable))
    }

    /// Evidence a registration carries: on the recording, ≥ 5-waypoint matches were wrong ~0.4 %
    /// of the time, 3–4-waypoint ones ~5 % (wrong 4-point ones nearly all had terrain < 0.1).
    pub fn evidence_points(&self, r: &ViewRegistration) -> u32 {
        if !r.terrain_only && r.inliers_count >= self.cfg.strong_inliers && r.rmse <= self.cfg.strong_max_rmse {
            4
        } else if r.inliers_count >= 5
            || (r.inliers_count == 4 && r.terrain_score >= self.cfg.four_point_min_terrain)
        {
            2
        } else {
            1
        }
    }

    /// Offers a registration computed on frame `frame_seq` (may be older than the current one).
    pub fn on_registration(&mut self, frame_seq: u64, reg: &ViewRegistration) -> RegistrationVerdict {
        if !reg.is_confident() {
            return RegistrationVerdict::NotConfident;
        }
        let Some((now, reliable)) = self.carry_forward(ViewTransform::from_registration(reg), frame_seq) else {
            return RegistrationVerdict::Stale;
        };
        let (w, h) = self.frame_dims;
        let agree_px = self.cfg.agree_px_1440;
        let agrees = |a: &ViewTransform, b: &ViewTransform| a.disagreement_px_1440(b, w, h) <= agree_px;

        if let Some(s) = self.state
            && agrees(&now, &s)
        {
            if reliable {
                self.anchor(now);
            }
            self.candidate = None;
            return RegistrationVerdict::Agrees;
        }
        if self.state.is_none()
            && reliable
            && let Some((hidden, _)) = self.hidden
            && agrees(&now, &hidden)
        {
            self.anchor(now);
            return RegistrationVerdict::Restored;
        }

        let p = self.evidence_points(reg);
        let verdict = match self.candidate.as_mut() {
            Some(c) if agrees(&now, &c.t) => {
                let distinct = c.moved_px_1440 >= self.cfg.distinct_view_px_1440
                    || c.zoom.ln().abs() >= self.cfg.distinct_view_zoom;
                let independent = distinct || reg.inliers_count >= self.cfg.same_view_min_inliers;
                c.points = if independent { c.points + p } else { c.points.max(p) };
                if distinct {
                    c.moved_px_1440 = 0.0;
                    c.zoom = 1.0;
                }
                if reliable {
                    c.t = now;
                }
                RegistrationVerdict::CandidateReinforced
            }
            _ => {
                self.candidate = Some(Candidate {
                    t: now,
                    points: p,
                    moved_px_1440: 0.0,
                    zoom: 1.0,
                });
                RegistrationVerdict::CandidateStarted
            }
        };
        // A registration carried over weak motions never makes anything visible: two of them
        // agree trivially on the shared (unverified) tail.
        let need = if self.state.is_some() { self.cfg.switch_points } else { self.cfg.acquire_points };
        if reliable
            && let Some(c) = self.candidate
            && c.points >= need
        {
            let switched = self.state.is_some();
            self.anchor(c.t);
            return if switched { RegistrationVerdict::Switched } else { RegistrationVerdict::Acquired };
        }
        verdict
    }

    fn anchor(&mut self, t: ViewTransform) {
        self.state = Some(t);
        self.hidden = None;
        self.candidate = None;
        self.frames_since_anchor = 0;
        self.anchored_this_frame = true;
    }

    pub fn output(&self) -> TrackerOutput {
        TrackerOutput {
            transform: self.state,
            source: self.state.map(|_| {
                if self.anchored_this_frame { TrackSource::Anchored } else { TrackSource::Tracked }
            }),
            frames_since_anchor: self.frames_since_anchor,
        }
    }
}

/// Map drawer (the "Bản Đồ" map switcher sliding in from the right) panel colour, BGR. While it
/// is open it covers the right ~third of the M-Map; the tracker keeps tracking the map behind it,
/// but guidance drawn there would sit on top of the drawer, so the app hides the overlay.
/// Measured on the e5 recording: 79–80 % of the strip below within ±8 with the drawer open,
/// ≤ 4 % on map views (dark sea / fog reach ~12 % only at ±14).
const DRAWER_BGR: [i16; 3] = [61, 48, 37];
const DRAWER_TOL: i16 = 8;
const DRAWER_MIN_FRACTION: f64 = 0.40;

/// Whether the M-Map's map drawer is open (see `DRAWER_BGR`). Samples the strip
/// x 0.68–0.985 w, y 0.12–0.88 h on a ~8 px @1440p grid.
pub fn is_map_drawer_open(frame_bgr: ArrayView3<'_, u8>) -> bool {
    let (h, w, c) = frame_bgr.dim();
    if c < 3 || w < 64 || h < 64 {
        return false;
    }
    let (x0, x1) = ((w as f64 * 0.68) as usize, (w as f64 * 0.985) as usize);
    let (y0, y1) = ((h as f64 * 0.12) as usize, (h as f64 * 0.88) as usize);
    let step = (h / 180).max(1);
    let (mut hit, mut n) = (0u32, 0u32);
    for y in (y0..y1).step_by(step) {
        let row = frame_bgr.slice(ndarray::s![y, .., ..]);
        if let Some(px) = row.as_slice() {
            for x in (x0..x1).step_by(step) {
                let off = x * c;
                n += 1;
                if (i16::from(px[off]) - DRAWER_BGR[0]).abs() <= DRAWER_TOL
                    && (i16::from(px[off + 1]) - DRAWER_BGR[1]).abs() <= DRAWER_TOL
                    && (i16::from(px[off + 2]) - DRAWER_BGR[2]).abs() <= DRAWER_TOL
                {
                    hit += 1;
                }
            }
        } else {
            for x in (x0..x1).step_by(step) {
                n += 1;
                if (0..3).all(|k| (i16::from(row[[x, k]]) - DRAWER_BGR[k]).abs() <= DRAWER_TOL) {
                    hit += 1;
                }
            }
        }
    }
    n > 0 && f64::from(hit) >= DRAWER_MIN_FRACTION * f64::from(n)
}

/// Content magnification implied by two consecutive slider readings (`None` across map changes).
pub fn slider_zoom_ratio(prev: Option<SliderScale>, cur: Option<SliderScale>) -> Option<f64> {
    match (prev, cur) {
        (Some(a), Some(b)) if a.map_id == b.map_id && a.scale > 0.0 && b.scale > 0.0 => Some(a.scale / b.scale),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn map_drawer_detection_needs_the_panel_colour_over_most_of_the_right_strip() {
        let (w, h) = (1280, 720);
        let mut img = render(w, h, 0.0, 0.0, 1.0);
        assert!(!is_map_drawer_open(img.view()), "map view");
        for y in (h * 8 / 100)..(h * 92 / 100) {
            for x in (w * 66 / 100)..w {
                // Panel colour with slight noise, as captured.
                let n = ((x * 7 + y * 13) % 7) as u8;
                img[[y, x, 0]] = 58 + n;
                img[[y, x, 1]] = 45 + n;
                img[[y, x, 2]] = 34 + n;
            }
        }
        assert!(is_map_drawer_open(img.view()), "drawer open");
        // A dark-blue band over a fifth of the strip (sea / fog) is not the drawer.
        let mut img = render(w, h, 0.0, 0.0, 1.0);
        for y in (h * 12 / 100)..(h * 30 / 100) {
            for x in (w * 68 / 100)..w {
                img[[y, x, 0]] = 61;
                img[[y, x, 1]] = 48;
                img[[y, x, 2]] = 37;
            }
        }
        assert!(!is_map_drawer_open(img.view()));
    }

    #[test]
    fn bgr_frame_view_is_zero_copy_and_honours_the_stride() {
        use gh_core::frame::{FrameView, PixelFormat};
        let (w, h, stride) = (4usize, 3usize, 16usize);
        let mut data = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                for k in 0..3 {
                    data[y * stride + x * 3 + k] = (y * 100 + x * 10 + k) as u8;
                }
            }
        }
        let fv = FrameView { width: w as u32, height: h as u32, stride, format: PixelFormat::Bgr8, data: &data };
        let a = crate::ops::color::frame_view_as_bgr(fv).expect("view");
        assert_eq!(a.dim(), (3, 4, 3));
        assert_eq!(a[[2, 3, 1]], 231);
        assert_eq!(a.as_ptr(), data.as_ptr());
        let bgra = FrameView { format: PixelFormat::Bgra8, ..fv };
        assert!(crate::ops::color::frame_view_as_bgr(bgra).is_none());
        let short = FrameView { data: &data[..20], ..fv };
        assert!(crate::ops::color::frame_view_as_bgr(short).is_none());
    }

    /// Deterministic textured "map" (sum of sines + blobs) rendered at a sub-pixel offset/zoom.
    fn render(w: usize, h: usize, ox: f64, oy: f64, zoom: f64) -> Array3<u8> {
        let mut img = Array3::<u8>::zeros((h, w, 3));
        for y in 0..h {
            for x in 0..w {
                let (u, v) = ((x as f64 - ox) / zoom, (y as f64 - oy) / zoom);
                let val = 128.0
                    + 40.0 * (u * 0.013).sin() * (v * 0.017).cos()
                    + 30.0 * (u * 0.051 + v * 0.023).sin()
                    + 25.0 * ((u * 0.0071).sin() + (v * 0.0093).cos())
                    + 20.0 * ((u * 0.11).sin() * (v * 0.07).sin());
                let g = val.clamp(0.0, 255.0) as u8;
                img[[y, x, 0]] = g;
                img[[y, x, 1]] = g;
                img[[y, x, 2]] = g;
            }
        }
        img
    }

    /// `render` zoomed by `zoom` about the frame centre, then panned by `(px, py)`.
    fn render_zoomed(w: usize, h: usize, zoom: f64, px: f64, py: f64) -> Array3<u8> {
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        render(w, h, (1.0 - zoom) * cx + px, (1.0 - zoom) * cy + py, zoom)
    }

    #[test]
    fn shift_estimator_recovers_translation_and_zoom() {
        let (w, h) = (1280, 720);
        let mut est = ShiftEstimator::new();
        assert!(est.push_frame(render(w, h, 0.0, 0.0, 1.0).view(), None, None).is_none());
        let m = est.push_frame(render(w, h, 37.0, -21.0, 1.0).view(), None, None).expect("motion");
        assert!((m.ex - 37.0).abs() < 2.5 && (m.ey + 21.0).abs() < 2.5, "{m:?}");
        assert!(m.peak > 0.15, "{m:?}");

        // Zoom 1.08x about the frame centre plus a pan: p_B = r·p_A + e.
        let r = 1.08;
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let mut est = ShiftEstimator::new();
        est.push_frame(render(w, h, 0.0, 0.0, 1.0).view(), None, None);
        let (ox, oy) = ((1.0 - r) * cx + 15.0, (1.0 - r) * cy - 9.0);
        let m = est.push_frame(render(w, h, ox, oy, r).view(), Some(r), None).expect("zoom motion");
        assert!((m.ex - ox).abs() < 3.0 && (m.ey - oy).abs() < 3.0, "{m:?} vs ({ox}, {oy})");
    }

    #[test]
    fn shift_estimator_follows_content_that_lags_the_zoom_slider() {
        // The slider reports the whole 0.88x zoom-out on frame 1; the content follows over two
        // frames (0.95x, then 0.88/0.95x), as recorded in game.
        let (w, h) = (1280, 720);
        let mut est = ShiftEstimator::new();
        est.push_frame(render_zoomed(w, h, 1.0, 0.0, 0.0).view(), None, None);
        let m1 = est.push_frame(render_zoomed(w, h, 0.95, 0.0, 0.0).view(), Some(0.88), None).expect("m1");
        assert!((m1.r - 0.95).abs() < 0.02, "frame 1 should apply ~0.95, got {m1:?}");
        let m2 = est.push_frame(render_zoomed(w, h, 0.88, 0.0, 0.0).view(), Some(1.0), None).expect("m2");
        assert!((m1.r * m2.r - 0.88).abs() < 0.015, "total zoom should be ~0.88: {m1:?} {m2:?}");
    }

    /// Low-texture "sea" (contrast scaled by `amp`) plus a bright 24 px cursor at `cursor`.
    fn render_sea_with_cursor(w: usize, h: usize, ox: f64, oy: f64, amp: f64, cursor: (usize, usize)) -> Array3<u8> {
        let mut img = render(w, h, ox, oy, 1.0);
        img.mapv_inplace(|v| (40.0 + (f64::from(v) - 128.0) * amp).clamp(0.0, 255.0) as u8);
        for y in cursor.1..(cursor.1 + 24).min(h) {
            for x in cursor.0..(cursor.0 + 24).min(w) {
                img[[y, x, 0]] = 0;
                img[[y, x, 1]] = 190;
                img[[y, x, 2]] = 255;
            }
        }
        img
    }

    #[test]
    fn cursor_mask_keeps_a_moving_cursor_from_hijacking_low_texture_motion() {
        let (w, h) = (1280, 720);
        let (a, b) = ((500, 300), (575, 350)); // cursor moves (+75, +50) px
        let frame_a = render_sea_with_cursor(w, h, 0.0, 0.0, 0.08, a);
        let frame_b = render_sea_with_cursor(w, h, 10.0, 5.0, 0.08, b); // map moves (+10, +5)
        let mut est = ShiftEstimator::new();
        est.push_frame(frame_a.view(), None, None);
        let raw = est.push_frame(frame_b.view(), None, None).expect("motion");
        assert!((raw.ex - 75.0).abs() < 6.0 && (raw.ey - 50.0).abs() < 6.0, "unmasked should follow the cursor: {raw:?}");
        let mut est = ShiftEstimator::new();
        est.push_frame(frame_a.view(), None, Some((a.0 as f64, a.1 as f64)));
        let m = est.push_frame(frame_b.view(), None, Some((b.0 as f64, b.1 as f64))).expect("masked motion");
        assert!((m.ex - 10.0).abs() < 2.5 && (m.ey - 5.0).abs() < 2.5, "masked should follow the map: {m:?}");
    }

    fn reg(tx: f64, ty: f64, inliers: usize) -> ViewRegistration {
        ViewRegistration {
            map_id: 2,
            scale: 1.0,
            tx,
            ty,
            inliers_count: inliers,
            rmse: 2.0,
            terrain_score: 0.4,
            terrain_only: false,
            zoom_ratio_t: Some(0.5),
            is_underground: false,
            candidates_count: 12,
        }
    }

    const W: usize = 1280;
    const H: usize = 720;

    fn mv(ex: f64, ey: f64, peak: f32) -> Option<ViewMotion> {
        Some(ViewMotion { r: 1.0, ex, ey, peak })
    }

    #[test]
    fn strong_registration_acquires_and_motion_carries_it() {
        let mut tr = ViewTracker::default();
        let s0 = tr.on_motion(None, W, H);
        assert_eq!(tr.on_registration(s0, &reg(100.0, 50.0, 9)), RegistrationVerdict::Acquired);
        tr.on_motion(mv(30.0, 0.0, 0.9), W, H); // content +30 px → tx −30 (scale 1)
        let t = tr.output().transform.expect("tracked");
        assert!((t.tx - 70.0).abs() < 1e-9, "{t:?}");
        assert_eq!(tr.output().source, Some(TrackSource::Tracked));
    }

    #[test]
    fn repeated_weak_matches_of_a_static_view_never_show_but_distinct_views_do() {
        let mut tr = ViewTracker::default();
        let s = tr.on_motion(None, W, H);
        assert_eq!(tr.on_registration(s, &reg(100.0, 50.0, 3)), RegistrationVerdict::CandidateStarted);
        for _ in 0..5 {
            let s = tr.on_motion(mv(0.0, 0.0, 0.99), W, H);
            tr.on_registration(s, &reg(100.0, 50.0, 3));
            assert!(tr.output().transform.is_none(), "same view, 1-point matches must not add up");
        }
        // The view moves 60 px between registrations → independent evidence accumulates.
        for i in 1..=3 {
            let s = tr.on_motion(mv(60.0, 0.0, 0.9), W, H);
            tr.on_registration(s, &reg(100.0 - 60.0 * f64::from(i), 50.0, 3));
        }
        let t = tr.output().transform.expect("acquired after 4 distinct views");
        assert!((t.tx + 80.0).abs() < 1e-6, "{t:?}");
    }

    #[test]
    fn four_point_matches_of_a_static_view_do_not_add_up() {
        let mut tr = ViewTracker::default();
        let s = tr.on_motion(None, W, H);
        tr.on_registration(s, &reg(100.0, 50.0, 4));
        let s = tr.on_motion(mv(0.0, 0.0, 0.99), W, H);
        assert_eq!(tr.on_registration(s, &reg(100.0, 50.0, 4)), RegistrationVerdict::CandidateReinforced);
        assert!(tr.output().transform.is_none(), "two 4-point matches of one static view are one piece of evidence");
        // A 5-point match of the same view is independent enough to add up (2 + 2).
        assert_eq!(tr.on_registration(s, &reg(100.0, 50.0, 5)), RegistrationVerdict::Acquired);
    }

    #[test]
    fn slider_ratio_is_only_used_within_one_map() {
        let a = Some(SliderScale { map_id: 36, scale: 1.2 });
        let b = Some(SliderScale { map_id: 7, scale: 0.8 });
        assert_eq!(slider_zoom_ratio(a, b), None);
        let c = Some(SliderScale { map_id: 7, scale: 0.75 });
        assert!((slider_zoom_ratio(b, c).expect("same map") - 0.8 / 0.75).abs() < 1e-12);
    }

    #[test]
    fn shown_transform_needs_switch_points_to_be_replaced() {
        let mut tr = ViewTracker::default();
        let s = tr.on_motion(None, W, H);
        tr.on_registration(s, &reg(100.0, 50.0, 9));
        // Two medium (5-point) conflicting matches = 4 points < 6: kept.
        let s = tr.on_motion(mv(0.0, 0.0, 0.99), W, H);
        assert_eq!(tr.on_registration(s, &reg(5000.0, 50.0, 5)), RegistrationVerdict::CandidateStarted);
        assert_eq!(tr.on_registration(s, &reg(5000.0, 50.0, 5)), RegistrationVerdict::CandidateReinforced);
        assert!((tr.output().transform.expect("kept").tx - 100.0).abs() < 1e-9);
        // An agreeing registration clears the conflicting candidate.
        assert_eq!(tr.on_registration(s, &reg(101.0, 50.0, 6)), RegistrationVerdict::Agrees);
        // A strong conflict plus a medium one (4 + 2) switches.
        tr.on_registration(s, &reg(5000.0, 50.0, 9));
        assert_eq!(tr.on_registration(s, &reg(5000.0, 50.0, 5)), RegistrationVerdict::Switched);
        assert!((tr.output().transform.expect("switched").tx - 5000.0).abs() < 1e-9);
    }

    #[test]
    fn weak_motion_hides_unless_it_continues_the_velocity_and_one_registration_restores() {
        let mut tr = ViewTracker::default();
        let s = tr.on_motion(None, W, H);
        tr.on_registration(s, &reg(0.0, 0.0, 9));
        tr.on_motion(mv(40.0, 0.0, 0.5), W, H);
        // Weak peak but continuing the drag velocity: still shown.
        tr.on_motion(mv(44.0, 2.0, 0.06), W, H);
        assert!(tr.output().transform.is_some());
        // Weak peak with an implausible jump: hidden, not dropped.
        tr.on_motion(mv(-200.0, 150.0, 0.06), W, H);
        assert!(tr.output().transform.is_none());
        let s = tr.on_motion(mv(0.0, 0.0, 0.9), W, H);
        let hidden_tx = -84.0 + 200.0;
        assert_eq!(tr.on_registration(s, &reg(hidden_tx, -2.0 - 150.0, 3)), RegistrationVerdict::Restored);
        // A registration carried over the weak motion cannot show anything by itself.
        let mut tr2 = ViewTracker::default();
        let s0 = tr2.on_motion(None, W, H);
        tr2.on_motion(mv(-200.0, 150.0, 0.06), W, H);
        tr2.on_registration(s0, &reg(0.0, 0.0, 12));
        assert!(tr2.output().transform.is_none(), "weak-carried strong match must stay hidden");
        // …until a reliably-carried one agrees with it.
        let s2 = tr2.on_motion(mv(0.0, 0.0, 0.9), W, H);
        assert_eq!(tr2.on_registration(s2, &reg(200.0, -150.0, 5)), RegistrationVerdict::Acquired);
    }

    #[test]
    fn late_registration_is_carried_forward_and_lost_motion_makes_it_stale() {
        let mut tr = ViewTracker::default();
        let s0 = tr.on_motion(None, W, H);
        let s1 = tr.on_motion(mv(10.0, 0.0, 0.9), W, H);
        tr.on_motion(mv(10.0, 0.0, 0.9), W, H);
        assert_eq!(tr.on_registration(s1, &reg(100.0, 0.0, 9)), RegistrationVerdict::Acquired);
        assert!((tr.output().transform.expect("carried").tx - 90.0).abs() < 1e-9);
        assert!(tr.frame_is_stable());
        tr.on_motion(None, W, H);
        assert!(!tr.frame_is_stable(), "no registration should start on a lost (transition) frame");
        assert_eq!(tr.on_registration(s0, &reg(100.0, 0.0, 9)), RegistrationVerdict::Stale);
    }

    #[test]
    fn tracker_on_frames_end_to_end() {
        let mut tr = ViewTracker::default();
        let s0 = tr.on_frame(render(W, H, 0.0, 0.0, 1.0).view(), None, None);
        assert_eq!(tr.on_registration(s0, &reg(100.0, 50.0, 9)), RegistrationVerdict::Acquired);
        tr.on_frame(render(W, H, 20.0, 0.0, 1.0).view(), None, None);
        tr.on_frame(render(W, H, 50.0, 0.0, 1.0).view(), None, None);
        let t = tr.output().transform.expect("tracked");
        assert!((t.tx - 50.0).abs() < 3.0, "{t:?}");
    }

    #[test]
    fn shift_estimator_is_scale_invariant_across_resolutions_and_bgra() {
        // Broadband multi-scale procedural map texture (realistic spatial bandwidth unlike the
        // 4-sine `render` helper, which at 1:1 640×360 has 99.8% empty high-frequency bins
        // dominated by unaveraged u8 truncation noise on fractional shifts/zoom).
        let render_map = |w: usize, h: usize, ox: f64, oy: f64, zoom: f64| {
            let mut img = Array3::<u8>::zeros((h, w, 3));
            let hash = |ix: i32, iy: i32| -> f64 {
                let mut z = (ix.wrapping_mul(374_761_393) ^ iy.wrapping_mul(668_265_263)) as u32;
                z = (z ^ (z >> 13)).wrapping_mul(1_274_126_177);
                ((z ^ (z >> 16)) & 0xFFFF) as f64 / 65535.0
            };
            let sample_grid = |u: f64, v: f64, cell: f64| -> f64 {
                let gx = (u / cell).floor();
                let gy = (v / cell).floor();
                let fx = u / cell - gx;
                let fy = v / cell - gy;
                let (ix, iy) = (gx as i32, gy as i32);
                let top = hash(ix, iy) * (1.0 - fx) + hash(ix + 1, iy) * fx;
                let bot = hash(ix, iy + 1) * (1.0 - fx) + hash(ix + 1, iy + 1) * fx;
                top * (1.0 - fy) + bot * fy
            };
            for y in 0..h {
                for x in 0..w {
                    let (u, v) = ((x as f64 - ox) / zoom, (y as f64 - oy) / zoom);
                    let val = 20.0
                        + 90.0 * sample_grid(u, v, 48.0)
                        + 75.0 * sample_grid(u, v, 16.0)
                        + 50.0 * sample_grid(u, v, 6.0);
                    let g = val.clamp(0.0, 255.0) as u8;
                    img[[y, x, 0]] = g;
                    img[[y, x, 1]] = g;
                    img[[y, x, 2]] = g;
                }
            }
            img
        };

        // Reuse one ShiftEstimator across multiple resolutions (rebuilding cached `x_bins` on
        // resize) and verify both fractional pan and zoom scale consistently with CANVAS_W×CANVAS_H.
        let mut est = ShiftEstimator::new();
        for (w, h, tol) in [
            (640usize, 360usize, 1.5f64),
            (1280, 720, 2.0),
            (1920, 1080, 3.0),
            (2560, 1440, 4.0),
        ] {
            let s = h as f64 / 720.0;
            let (tx, ty) = (37.0 * s, -21.0 * s);
            assert!(
                est.push_frame(render_map(w, h, 0.0, 0.0, s).view(), None, None).is_none(),
                "resolution switch to {w}x{h} must reset and return None"
            );
            let m = est.push_frame(render_map(w, h, tx, ty, s).view(), None, None).expect("pan");
            assert!(
                (m.ex - tx).abs() < tol && (m.ey - ty).abs() < tol && m.peak > 0.15,
                "{w}x{h} pan mismatch: {m:?} vs ({tx}, {ty})"
            );

            // Zoom + pan at the same resolution on a fresh reference.
            est.reset();
            let r = 1.08;
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let (ox, oy) = ((1.0 - r) * cx + 15.0 * s, (1.0 - r) * cy - 9.0 * s);
            assert!(est.push_frame(render_map(w, h, 0.0, 0.0, s).view(), None, None).is_none());
            let mz = est
                .push_frame(render_map(w, h, ox, oy, r * s).view(), Some(r), None)
                .expect("zoom");
            assert!(
                (mz.ex - ox).abs() < tol + 1.0 && (mz.ey - oy).abs() < tol + 1.0 && mz.peak > 0.15,
                "{w}x{h} zoom mismatch: {mz:?} vs ({ox}, {oy})"
            );
        }

        // 4-channel BGRA input must yield identical motion to 3-channel BGR.
        let (w, h) = (1280, 720);
        let a_bgr = render(w, h, 0.0, 0.0, 1.0);
        let b_bgr = render(w, h, 24.0, -14.0, 1.0);
        let to_bgra = |src: &Array3<u8>, alpha: u8| {
            let mut out = Array3::<u8>::zeros((h, w, 4));
            for y in 0..h {
                for x in 0..w {
                    out[[y, x, 0]] = src[[y, x, 0]];
                    out[[y, x, 1]] = src[[y, x, 1]];
                    out[[y, x, 2]] = src[[y, x, 2]];
                    out[[y, x, 3]] = alpha;
                }
            }
            out
        };
        let a_bgra = to_bgra(&a_bgr, 255);
        let b_bgra = to_bgra(&b_bgr, 128);
        let mut e_bgr = ShiftEstimator::new();
        e_bgr.push_frame(a_bgr.view(), None, None);
        let m_bgr = e_bgr.push_frame(b_bgr.view(), None, None).expect("bgr");
        let mut e_bgra = ShiftEstimator::new();
        e_bgra.push_frame(a_bgra.view(), None, None);
        let m_bgra = e_bgra.push_frame(b_bgra.view(), None, None).expect("bgra");
        assert!((m_bgra.ex - m_bgr.ex).abs() < 1e-6, "{m_bgra:?} vs {m_bgr:?}");
        assert!((m_bgra.ey - m_bgr.ey).abs() < 1e-6, "{m_bgra:?} vs {m_bgr:?}");
        assert!((m_bgra.peak - m_bgr.peak).abs() < 1e-6, "{m_bgra:?} vs {m_bgr:?}");
    }
}
