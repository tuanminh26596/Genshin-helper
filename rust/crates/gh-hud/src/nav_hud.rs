//! 2-layer pre-baked `NavHudRenderer`, `SpringAngleSmoother`, `ScalarSmoother`,
//! `CircularAngleSmoother`, Guide Card zoom modes (`cycle` / `compact` / `large`),
//! and `NavMiniHud` Win32 layered host (`src/nav_hud.py`).

use crate::fonts::{FontWeightRole, HudFonts, TextAlign, TextEffect, TextStyle};
use crate::layered::{
    LayeredHandler, LayeredWindowConfig, LayeredWindowCtx, LayeredWindowHost, WDA_REFRESH_MS,
};
use crate::toast::build_rounded_rect_path;
use crate::{HudError, HudResult};
use cosmic_text::{Ellipsize, Wrap};
use gh_core::frame::RectI32;
use gh_core::i18n::{get_language, t_lang};
use gh_core::nav_controller::{NavGuidance, NavState};
use gh_core::ui_layout::compute_ui_layout;
use gh_win32::capture::WindowFinder;
use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, Delay, ImageDecoder, ImageFormat, ImageReader, RgbaImage};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap,
    PixmapMut, PixmapPaint, PremultipliedColorU8, Stroke, Transform,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetCursorPos, WM_APP};

// ---------------------------------------------------------------------------
// Public Constants
// ---------------------------------------------------------------------------

pub const POSITION_NOISE_M: f64 = 1.4;
pub const RADAR_RANGE_M: f64 = 25.0;
pub const COMPACT_DIST_THRESHOLD_M: f64 = 14.0;
pub const LOCK_ON_DEG: f64 = 12.0;
pub const BEHIND_DEG: f64 = 135.0;
pub const BADGE_STEP_DEG: i32 = 4;
pub const BADGE_COUNT: i32 = 90;
pub const MAX_BADGE_CACHE_BYTES: usize = 480 * 1024; // 491_520 bytes (< 0.5 MB)
pub const MAX_CARD_CACHE_ENTRIES: usize = 32;
pub const MAX_THUMB_CACHE_ENTRIES: usize = 30;
pub const MAX_GUIDE_CACHE_ENTRIES: usize = 6;
pub const MAX_GUIDE_ANIM_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_GUIDE_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RADAR_CACHE_ENTRIES: usize = 16;
pub const THUMB_SIZE: (u32, u32) = (72, 54);
pub const GUIDE_IMAGE_MAX: (u32, u32) = (960, 600);

pub const POSITION_MODES: [&str; 5] = [
    "under_minimap",
    "right_of_minimap",
    "top_center",
    "bottom_center",
    "custom",
];
pub const GUIDE_ZOOM_MODES: [&str; 3] = ["cycle", "compact", "large"];

pub const WM_APP_UPDATE: u32 = WM_APP + 0x11; // 0x8011
pub const WM_APP_HIDE: u32 = WM_APP + 0x12; // 0x8012
pub const WM_APP_CLOSE: u32 = WM_APP + 0x13; // 0x8013
pub const WM_APP_EDIT: u32 = WM_APP + 0x14; // 0x8014

pub const TIMER_RENDER: usize = 101;
pub const TIMER_GUIDE_ANIM: usize = 103;
pub const RENDER_TICK_MS: u32 = 33;
pub const EDIT_TIMEOUT_S: f64 = 120.0;
pub const HOVER_FADE_FULL_ALPHA: u8 = 255;
pub const HOVER_FADE_MIN_ALPHA: u8 = 78;
pub const HOVER_FADE_STEP: u8 = 22;

const MAX_DECODE_DIM: u32 = 8192;
const MAX_DECODE_PIXELS: u64 = 64_000_000;
const MAX_ANIM_FRAME_PIXELS: u64 = 4_000_000;
const MAX_GUIDE_DIM_CLAMP: u32 = 4096;
const MIN_ANIM_FRAME_DELAY_MS: u32 = 10;
const DEFAULT_FAST_GIF_DELAY_MS: u32 = 100;
const MAX_ANIM_FRAME_DELAY_MS: u32 = 5_000;

// Teyvat Craft obsidian glass palette (`alpha >= 240` on pill & dial interiors)
const GLASS_BG: [u8; 4] = [26, 30, 40, 242];
const DIAL_BG: [u8; 4] = [20, 24, 34, 246];
const AMBER_RING: [u8; 4] = [232, 168, 56, 242];
const SPOT_BORDER: [u8; 4] = [211, 188, 142, 130];

// ---------------------------------------------------------------------------
// Enums & Palettes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PositionMode {
    #[default]
    UnderMinimap,
    RightOfMinimap,
    TopCenter,
    BottomCenter,
    Custom,
}

impl PositionMode {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "right_of_minimap" => Self::RightOfMinimap,
            "top_center" => Self::TopCenter,
            "bottom_center" => Self::BottomCenter,
            "custom" => Self::Custom,
            _ => Self::UnderMinimap,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnderMinimap => "under_minimap",
            Self::RightOfMinimap => "right_of_minimap",
            Self::TopCenter => "top_center",
            Self::BottomCenter => "bottom_center",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GuideZoomMode {
    #[default]
    Cycle,
    Compact,
    Large,
}

impl GuideZoomMode {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "compact" => Self::Compact,
            "large" => Self::Large,
            _ => Self::Cycle,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cycle => "cycle",
            Self::Compact => "compact",
            Self::Large => "large",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HudPalette {
    #[default]
    Info,
    Success,
    Warning,
    LockOn,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteColors {
    pub accent: [u8; 4],
    pub border: [u8; 4],
    pub badge_tint: [u8; 4],
    pub tag_bg: [u8; 4],
    pub tag_text: [u8; 4],
    pub title: [u8; 4],
    pub sub: [u8; 4],
}

impl HudPalette {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "success" => Self::Success,
            "warning" => Self::Warning,
            "lock_on" => Self::LockOn,
            "edit" => Self::Edit,
            _ => Self::Info,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::LockOn => "lock_on",
            Self::Edit => "edit",
        }
    }

    pub const fn colors(self) -> PaletteColors {
        match self {
            Self::Info => PaletteColors {
                accent: [211, 188, 142, 255],
                border: [211, 188, 142, 195],
                badge_tint: [211, 188, 142, 34],
                tag_bg: [43, 50, 68, 225],
                tag_text: [223, 196, 140, 255],
                title: [242, 236, 224, 255],
                sub: [184, 175, 158, 255],
            },
            Self::Success => PaletteColors {
                accent: [82, 196, 160, 255],
                border: [211, 188, 142, 200],
                badge_tint: [82, 196, 160, 40],
                tag_bg: [82, 196, 160, 48],
                tag_text: [167, 243, 208, 255],
                title: [242, 236, 224, 255],
                sub: [184, 175, 158, 255],
            },
            Self::Warning => PaletteColors {
                accent: [224, 171, 88, 255],
                border: [224, 171, 88, 210],
                badge_tint: [224, 171, 88, 44],
                tag_bg: [224, 171, 88, 52],
                tag_text: [246, 224, 180, 255],
                title: [242, 236, 224, 255],
                sub: [216, 196, 154, 255],
            },
            Self::LockOn => PaletteColors {
                accent: [45, 212, 191, 255],
                border: [211, 188, 142, 225],
                badge_tint: [45, 212, 191, 48],
                tag_bg: [45, 212, 191, 52],
                tag_text: [165, 243, 252, 255],
                title: [242, 236, 224, 255],
                sub: [184, 175, 158, 255],
            },
            Self::Edit => PaletteColors {
                accent: [223, 196, 140, 255],
                border: [223, 196, 140, 235],
                badge_tint: [211, 188, 142, 54],
                tag_bg: [211, 188, 142, 56],
                tag_text: [245, 231, 196, 255],
                title: [242, 236, 224, 255],
                sub: [211, 188, 142, 255],
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Angle Smoothers & Deadband Math
// ---------------------------------------------------------------------------

/// Name of the map site a pin's guide comes from, for the Guide card's loading / empty / error text.
/// Third-party pins are namespaced (`appsample:<id>`, `kongying:<id>`) and carry their own inline
/// guide; everything else is a HoYoLAB pin fetched by `PinGuideFetcher`.
/// The suggestion position to show, only while its pin is still the target.
fn guide_suggestion_for_target(
    raw: Option<&(String, usize, usize)>,
    guidance: &NavGuidance,
) -> Option<(usize, usize)> {
    let (pid, index, total) = raw?;
    let target = guidance.target.as_ref()?;
    (target.id == *pid).then_some((*index, *total))
}

pub fn guide_source_name(pin_id: &str) -> &'static str {
    if pin_id.starts_with("appsample:") {
        "AppSample"
    } else if pin_id.starts_with("kongying:") {
        "Kongying"
    } else {
        "HoYoLAB"
    }
}

#[inline]
pub(crate) fn wrap_deg_signed(deg: f64) -> f64 {
    ((deg + 180.0).rem_euclid(360.0)) - 180.0
}

/// Angular uncertainty caused by minimap position jitter at a given distance (`src/nav_hud.py`).
pub fn noise_deadband_deg(distance_m: Option<f64>, noise_m: f64) -> f64 {
    let Some(raw_d) = distance_m.filter(|d| d.is_finite()) else {
        return 2.0;
    };
    if !noise_m.is_finite() {
        return 2.0;
    }
    let d = raw_d.max(1.0);
    noise_m.atan2(d).to_degrees().clamp(1.2, 14.0)
}

/// Wrap-safe circular Exponential Moving Average smoother on `(sin θ, cos θ)`.
#[derive(Debug, Clone)]
pub struct CircularAngleSmoother {
    alpha: f64,
    deadband_deg: f64,
    sin: f64,
    cos: f64,
    value_deg: Option<f64>,
    target_deg: Option<f64>,
}

impl Default for CircularAngleSmoother {
    fn default() -> Self {
        Self::new(0.35, 1.5)
    }
}

impl CircularAngleSmoother {
    pub fn new(alpha: f64, deadband_deg: f64) -> Self {
        let a = if alpha.is_finite() {
            alpha.clamp(0.01, 1.0)
        } else {
            0.35
        };
        let db = if deadband_deg.is_finite() {
            deadband_deg.max(0.0)
        } else {
            1.5
        };
        Self {
            alpha: a,
            deadband_deg: db,
            sin: 0.0,
            cos: 1.0,
            value_deg: None,
            target_deg: None,
        }
    }

    pub const fn value(&self) -> Option<f64> {
        self.value_deg
    }

    pub const fn target(&self) -> Option<f64> {
        self.target_deg
    }

    pub fn reset(&mut self) {
        self.sin = 0.0;
        self.cos = 1.0;
        self.value_deg = None;
        self.target_deg = None;
    }

    pub fn set_target(&mut self, angle_deg: Option<f64>) {
        match angle_deg.filter(|a| a.is_finite()) {
            None => self.target_deg = None,
            Some(a) => self.target_deg = Some(wrap_deg_signed(a)),
        }
    }

    pub fn update(&mut self, angle_deg: Option<f64>) -> Option<f64> {
        self.set_target(angle_deg);
        self.step()
    }

    pub fn step(&mut self) -> Option<f64> {
        let Some(target) = self.target_deg else {
            return self.value_deg;
        };
        let rad = target.to_radians();
        let t_sin = rad.sin();
        let t_cos = rad.cos();

        let Some(cur) = self.value_deg else {
            self.sin = t_sin;
            self.cos = t_cos;
            self.value_deg = Some(target);
            return self.value_deg;
        };

        let diff = wrap_deg_signed(target - cur);
        if diff.abs() < self.deadband_deg {
            return self.value_deg;
        }

        let a = self.alpha;
        let mut ns = (1.0 - a) * self.sin + a * t_sin;
        let mut nc = (1.0 - a) * self.cos + a * t_cos;
        let mag = ns.hypot(nc);
        let dot = self.sin * t_sin + self.cos * t_cos;
        if mag < 1e-6 || diff.abs() > 179.0 || dot < -0.9998 {
            let step_rad = (cur + diff * a).to_radians();
            ns = step_rad.sin();
            nc = step_rad.cos();
        } else {
            ns /= mag;
            nc /= mag;
        }

        self.sin = ns;
        self.cos = nc;
        self.value_deg = Some(self.sin.atan2(self.cos).to_degrees());
        self.value_deg
    }
}

/// Wrap-safe critically-damped spring smoother for the HUD compass needle (`src/nav_hud.py`).
#[derive(Debug, Clone)]
pub struct SpringAngleSmoother {
    pub omega_far: f64,
    pub omega_near: f64,
    omega: f64,
    deadband: f64,
    value: Option<f64>,
    velocity: f64,
    goal: Option<f64>,
    last_ts_s: Option<f64>,
    origin: Instant,
}

impl Default for SpringAngleSmoother {
    fn default() -> Self {
        Self::new(14.0, 4.5)
    }
}

impl SpringAngleSmoother {
    pub const MAX_SPEED_DEG_S: f64 = 420.0;

    pub fn new(omega_far: f64, omega_near: f64) -> Self {
        let far = if omega_far.is_finite() {
            omega_far.max(0.1)
        } else {
            14.0
        };
        let near = if omega_near.is_finite() {
            omega_near.max(0.1)
        } else {
            4.5
        };
        Self {
            omega_far: far,
            omega_near: near,
            omega: far,
            deadband: 2.0,
            value: None,
            velocity: 0.0,
            goal: None,
            last_ts_s: None,
            origin: Instant::now(),
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value.map(wrap_deg_signed)
    }

    pub const fn goal(&self) -> Option<f64> {
        self.goal
    }

    pub const fn deadband(&self) -> f64 {
        self.deadband
    }

    pub const fn omega(&self) -> f64 {
        self.omega
    }

    pub fn reset(&mut self) {
        self.value = None;
        self.velocity = 0.0;
        self.goal = None;
        self.last_ts_s = None;
    }

    pub fn set_distance(&mut self, distance_m: Option<f64>) {
        self.deadband = noise_deadband_deg(distance_m, POSITION_NOISE_M);
        let Some(d) = distance_m.filter(|v| v.is_finite()) else {
            self.omega = self.omega_far;
            return;
        };
        let k = ((d - 12.0) / 48.0).clamp(0.0, 1.0);
        self.omega = self.omega_near + (self.omega_far - self.omega_near) * k;
    }

    pub fn set_target(&mut self, angle_deg: Option<f64>) {
        const LOCK_ON_DEG: f64 = 12.0;
        const LOCK_ON_HOLD_DEADBAND_DEG: f64 = 3.0;
        let Some(a) = angle_deg.filter(|v| v.is_finite()) else {
            self.goal = None;
            return;
        };
        let target = wrap_deg_signed(a);
        let Some(prev_goal) = self.goal else {
            self.goal = Some(target);
            return;
        };
        let diff = wrap_deg_signed(target - prev_goal);
        let effective_deadband =
            if prev_goal.abs() <= LOCK_ON_DEG && target.abs() <= LOCK_ON_DEG {
                self.deadband.max(LOCK_ON_HOLD_DEADBAND_DEG)
            } else {
                self.deadband
            };
        if diff.abs() >= effective_deadband {
            self.goal = Some(target);
        }
    }

    pub fn step(&mut self) -> Option<f64> {
        let now_s = self.origin.elapsed().as_secs_f64();
        self.step_at(now_s)
    }

    pub fn step_at(&mut self, now_s: f64) -> Option<f64> {
        if !now_s.is_finite() || self.goal.is_none() {
            return self.value();
        }
        let raw_dt = match self.last_ts_s {
            Some(prev) => now_s - prev,
            None => 1.0 / 30.0,
        };
        self.last_ts_s = Some(now_s);
        if raw_dt <= 0.0 && self.value.is_some() {
            return self.value();
        }
        let dt = if raw_dt <= 0.0 {
            1.0 / 30.0
        } else {
            raw_dt.clamp(1e-3, 0.1)
        };
        self.integrate(dt)
    }

    pub fn step_with_dt(&mut self, dt_s: f64) -> Option<f64> {
        if !dt_s.is_finite() || self.goal.is_none() {
            return self.value();
        }
        if self.value.is_none() {
            self.value = self.goal;
            self.velocity = 0.0;
            return self.value();
        }
        if dt_s <= 0.0 {
            return self.value();
        }
        self.integrate(dt_s.clamp(1e-3, 0.1))
    }

    fn integrate(&mut self, dt: f64) -> Option<f64> {
        let goal = self.goal?;
        let Some(cur) = self.value else {
            self.value = Some(goal);
            self.velocity = 0.0;
            return self.value();
        };

        let err = wrap_deg_signed(goal - cur);
        if err.abs() < 0.05 && self.velocity.abs() < 0.5 {
            return self.value();
        }
        let w = self.omega;
        let accel = w * w * err - 2.0 * w * self.velocity;
        self.velocity = (self.velocity + accel * dt).clamp(-Self::MAX_SPEED_DEG_S, Self::MAX_SPEED_DEG_S);
        self.value = Some(cur + self.velocity * dt);
        self.value()
    }
}

/// Frame-rate independent exponential smoother for distances (meters).
#[derive(Debug, Clone)]
pub struct ScalarSmoother {
    tau_s: f64,
    value: Option<f64>,
    last_ts_s: Option<f64>,
    origin: Instant,
}

impl Default for ScalarSmoother {
    fn default() -> Self {
        Self::new(0.35)
    }
}

impl ScalarSmoother {
    pub fn new(tau_s: f64) -> Self {
        let tau = if tau_s.is_finite() {
            tau_s.max(1e-3)
        } else {
            0.35
        };
        Self {
            tau_s: tau,
            value: None,
            last_ts_s: None,
            origin: Instant::now(),
        }
    }

    pub const fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn reset(&mut self) {
        self.value = None;
        self.last_ts_s = None;
    }

    pub fn update(&mut self, target: Option<f64>) -> Option<f64> {
        let now_s = self.origin.elapsed().as_secs_f64();
        self.update_at(target, now_s)
    }

    pub fn update_at(&mut self, target: Option<f64>, now_s: f64) -> Option<f64> {
        let Some(t) = target.filter(|v| v.is_finite()) else {
            return self.value;
        };
        if !now_s.is_finite() {
            return self.value;
        }
        match (self.value, self.last_ts_s) {
            (Some(cur), Some(prev)) if (t - cur).abs() <= 60.0 => {
                let dt = (now_s - prev).clamp(0.0, 0.5);
                let a = 1.0 - (-dt / self.tau_s).exp();
                self.value = Some(cur + (t - cur) * a);
            }
            _ => {
                self.value = Some(t);
            }
        }
        self.last_ts_s = Some(now_s);
        self.value
    }
}

/// Smooth cursor-hover auto-fade controller for the click-through Mini-HUD (distilled from
/// Blish-HUD MIT). Fades toward `faded_alpha` while the cursor hovers over the HUD rect and
/// restores toward `full_alpha` when leaving, staying opaque when suppressed (edit mode or
/// expanded guide card) or disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoverFadeController {
    full_alpha: u8,
    faded_alpha: u8,
    step: u8,
    current_alpha: u8,
    enabled: bool,
}

impl Default for HoverFadeController {
    fn default() -> Self {
        Self::new(HOVER_FADE_FULL_ALPHA, HOVER_FADE_MIN_ALPHA, HOVER_FADE_STEP)
    }
}

impl HoverFadeController {
    pub fn new(full_alpha: u8, faded_alpha: u8, step: u8) -> Self {
        let min_a = faded_alpha.min(full_alpha);
        Self {
            full_alpha,
            faded_alpha: min_a,
            step: step.max(1),
            current_alpha: full_alpha,
            enabled: true,
        }
    }

    pub const fn current_alpha(&self) -> u8 {
        self.current_alpha
    }

    pub const fn full_alpha(&self) -> u8 {
        self.full_alpha
    }

    pub const fn faded_alpha(&self) -> u8 {
        self.faded_alpha
    }

    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn reset(&mut self) {
        self.current_alpha = self.full_alpha;
    }

    /// Advances alpha one render tick (`RENDER_TICK_MS = 33ms`).
    /// Returns `Some(new_alpha)` iff the alpha value changed.
    pub fn tick(&mut self, hovered: bool, suppressed: bool) -> Option<u8> {
        if suppressed || !self.enabled {
            if self.current_alpha != self.full_alpha {
                self.current_alpha = self.full_alpha;
                return Some(self.current_alpha);
            }
            return None;
        }
        let target = if hovered {
            self.faded_alpha
        } else {
            self.full_alpha
        };
        let next = if self.current_alpha > target {
            self.current_alpha.saturating_sub(self.step).max(target)
        } else if self.current_alpha < target {
            self.current_alpha.saturating_add(self.step).min(target)
        } else {
            return None;
        };
        if next == self.current_alpha {
            return None;
        }
        self.current_alpha = next;
        Some(next)
    }
}

#[inline]
fn point_in_rect(px: i32, py: i32, rect: RectI32) -> bool {
    rect.width > 0
        && rect.height > 0
        && px >= rect.x
        && px < rect.x.saturating_add(rect.width)
        && py >= rect.y
        && py < rect.y.saturating_add(rect.height)
}

// ---------------------------------------------------------------------------
// Geometry & Premultiplied Helpers
// ---------------------------------------------------------------------------

#[inline]
fn rotate_point(x: f32, y: f32, angle_deg_cw: f32) -> (f32, f32) {
    let rad = angle_deg_cw.to_radians();
    let c = rad.cos();
    let s = rad.sin();
    (x * c - y * s, x * s + y * c)
}

#[inline]
fn make_paint(rgba: [u8; 4]) -> Paint<'static> {
    let mut p = Paint {
        anti_alias: true,
        ..Default::default()
    };
    p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    p
}

fn build_polygon_path(pts: &[(f32, f32)]) -> Option<Path> {
    if pts.len() < 3 {
        return None;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    for &(x, y) in &pts[1..] {
        pb.line_to(x, y);
    }
    pb.close();
    pb.finish()
}

fn build_circle_stroke_path(cx: f32, cy: f32, outer_r: f32, stroke_w: f32) -> Option<Path> {
    let r = (outer_r - stroke_w * 0.5).max(0.5);
    PathBuilder::from_circle(cx, cy, r)
}

fn rgba_image_to_premultiplied_pixmap_into(img: &RgbaImage, dst_pm: &mut Pixmap) -> bool {
    if img.width() != dst_pm.width() || img.height() != dst_pm.height() {
        return false;
    }
    let src = img.as_raw();
    let dst = dst_pm.pixels_mut();
    for (chunk, px) in src.as_chunks::<4>().0.iter().zip(dst.iter_mut()) {
        let a = chunk[3] as u16;
        let r = ((chunk[0] as u16 * a + 127) / 255) as u8;
        let g = ((chunk[1] as u16 * a + 127) / 255) as u8;
        let b = ((chunk[2] as u16 * a + 127) / 255) as u8;
        if let Some(c) = PremultipliedColorU8::from_rgba(r, g, b, chunk[3]) {
            *px = c;
        }
    }
    true
}

fn rgba_image_to_premultiplied_pixmap(img: &RgbaImage) -> Option<Pixmap> {
    let mut pm = Pixmap::new(img.width(), img.height())?;
    if rgba_image_to_premultiplied_pixmap_into(img, &mut pm) {
        Some(pm)
    } else {
        None
    }
}

fn decode_image_bytes(bytes: &[u8]) -> Option<RgbaImage> {
    if bytes.is_empty() {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > MAX_DECODE_DIM || h > MAX_DECODE_DIM {
        return None;
    }
    if (w as u64).saturating_mul(h as u64) > MAX_DECODE_PIXELS {
        return None;
    }
    let reader2 = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let dyn_img = reader2.decode().ok()?;
    Some(dyn_img.to_rgba8())
}

fn normalize_thumbnail_rgba(src: &RgbaImage) -> Option<Pixmap> {
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 {
        return None;
    }
    let (tw, th) = THUMB_SIZE;
    // Aspect-preserving center-crop (ImageOps.fit equivalent)
    let target_ratio = tw as f64 / th as f64;
    let src_ratio = sw as f64 / sh as f64;
    let (cw, ch, cx, cy) = if src_ratio > target_ratio {
        let new_w = ((sh as f64 * target_ratio).round() as u32).clamp(1, sw);
        (new_w, sh, (sw - new_w) / 2, 0)
    } else {
        let new_h = ((sw as f64 / target_ratio).round() as u32).clamp(1, sh);
        (sw, new_h, 0, (sh - new_h) / 2)
    };
    let cropped = image::imageops::crop_imm(src, cx, cy, cw, ch).to_image();
    let fitted = image::imageops::resize(&cropped, tw, th, FilterType::Lanczos3);

    // Mask to rounded rectangle while preserving interior opacity
    let src_pm = rgba_image_to_premultiplied_pixmap(&fitted)?;
    let mut out = Pixmap::new(tw, th)?;
    let path = build_rounded_rect_path(0.0, 0.0, tw as f32, th as f32, 6.0)?;
    let mut mask = tiny_skia::Mask::new(tw, th)?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    out.draw_pixmap(
        0,
        0,
        src_pm.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        Some(&mask),
    );
    Some(out)
}

fn contain_guide_rgba(src: &RgbaImage) -> Option<RgbaImage> {
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 {
        return None;
    }
    let (mw, mh) = GUIDE_IMAGE_MAX;
    let scale = (mw as f64 / sw as f64).min(mh as f64 / sh as f64);
    let nw = ((sw as f64 * scale).round() as u32).clamp(1, mw);
    let nh = ((sh as f64 * scale).round() as u32).clamp(1, mh);
    if nw == sw && nh == sh {
        return Some(src.clone());
    }
    Some(image::imageops::resize(src, nw, nh, FilterType::Lanczos3))
}

fn fit_guide_max_dims(sw: u32, sh: u32) -> (u32, u32) {
    let (mw, mh) = GUIDE_IMAGE_MAX;
    let scale = (mw as f64 / sw.max(1) as f64)
        .min(mh as f64 / sh.max(1) as f64)
        .min(1.0);
    (
        ((sw.max(1) as f64 * scale).round() as u32).clamp(1, mw),
        ((sh.max(1) as f64 * scale).round() as u32).clamp(1, mh),
    )
}

fn write_rgba_to_premultiplied_pixmap(
    src: &RgbaImage,
    dst_w: u32,
    dst_h: u32,
    dst_pm: &mut Pixmap,
) -> bool {
    if src.width() == 0 || src.height() == 0 || dst_w == 0 || dst_h == 0 {
        return false;
    }
    if src.width() == dst_w && src.height() == dst_h {
        rgba_image_to_premultiplied_pixmap_into(src, dst_pm)
    } else {
        let scaled = image::imageops::resize(src, dst_w, dst_h, FilterType::Triangle);
        rgba_image_to_premultiplied_pixmap_into(&scaled, dst_pm)
    }
}

fn normalize_frame_delay_ms(delay: Delay) -> u32 {
    let (numer, denom) = delay.numer_denom_ms();
    let raw_ms = if denom == 0 {
        0
    } else {
        ((u64::from(numer) + u64::from(denom) / 2) / u64::from(denom))
            .min(u64::from(MAX_ANIM_FRAME_DELAY_MS)) as u32
    };
    if raw_ms <= MIN_ANIM_FRAME_DELAY_MS {
        DEFAULT_FAST_GIF_DELAY_MS
    } else {
        raw_ms.min(MAX_ANIM_FRAME_DELAY_MS)
    }
}

#[derive(Clone)]
struct SharedBytes(Arc<[u8]>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimatedFormat {
    Gif,
    WebP,
}

#[derive(Clone)]
struct AnimatedGuide {
    format: AnimatedFormat,
    width: u32,
    height: u32,
    bytes: Arc<[u8]>,
    first_frame: Arc<Pixmap>,
    first_delay_ms: u32,
}

struct SendFrames(image::Frames<'static>);

// SAFETY: `SendFrames` is only constructed by `AnimatedGuide::open_frames` from
// `GifDecoder<Cursor<SharedBytes>>` and `WebPDecoder<Cursor<SharedBytes>>`.
// `assert_anim_decoders_send` below verifies at compile time that both underlying
// decoders (and `RgbaImage` / `Limits`) implement `Send`, and their frame iterators
// add no `!Send` fields and are accessed exclusively under `&mut self`.
unsafe impl Send for SendFrames {}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<GifDecoder<Cursor<SharedBytes>>>();
    assert_send::<WebPDecoder<Cursor<SharedBytes>>>();
    assert_send::<RgbaImage>();
    assert_send::<image::Limits>();
};

impl AnimatedGuide {
    fn nbytes(&self) -> usize {
        self.bytes
            .len()
            .saturating_add(self.first_frame.data().len())
    }

    fn open_frames(&self) -> Option<SendFrames> {
        let cur = Cursor::new(SharedBytes(Arc::clone(&self.bytes)));
        match self.format {
            AnimatedFormat::Gif => Some(SendFrames(GifDecoder::new(cur).ok()?.into_frames())),
            AnimatedFormat::WebP => Some(SendFrames(WebPDecoder::new(cur).ok()?.into_frames())),
        }
    }
}

struct StreamingAnimPlayer {
    gen_id: u64,
    anim: Arc<AnimatedGuide>,
    frames_iter: Option<SendFrames>,
    canvas: Pixmap,
    frame_index: usize,
    current_delay_ms: u32,
}

impl StreamingAnimPlayer {
    fn new(gen_id: u64, anim: Arc<AnimatedGuide>) -> Self {
        let canvas = (*anim.first_frame).clone();
        let current_delay_ms = anim.first_delay_ms;
        Self {
            gen_id,
            anim,
            frames_iter: None,
            canvas,
            frame_index: 0,
            current_delay_ms,
        }
    }

    fn reset_to_first(&mut self) -> (usize, u32) {
        self.frames_iter = None;
        if self.canvas.data().len() == self.anim.first_frame.data().len() {
            self.canvas
                .data_mut()
                .copy_from_slice(self.anim.first_frame.data());
        } else {
            self.canvas = (*self.anim.first_frame).clone();
        }
        self.frame_index = 0;
        self.current_delay_ms = self.anim.first_delay_ms;
        (0, self.current_delay_ms)
    }

    fn step_next(&mut self) -> (usize, u32) {
        if self.frames_iter.is_none() {
            let Some(mut iter) = self.anim.open_frames() else {
                return self.reset_to_first();
            };
            // Frame 0 is already displayed in `canvas`; skip it so next() yields frame 1.
            if iter.0.next().is_none() {
                return self.reset_to_first();
            }
            self.frames_iter = Some(iter);
        }

        let next_res = self.frames_iter.as_mut().and_then(|it| it.0.next());
        match next_res {
            Some(Ok(frame)) => {
                let delay_ms = normalize_frame_delay_ms(frame.delay());
                let (cw, ch) = (self.canvas.width(), self.canvas.height());
                if write_rgba_to_premultiplied_pixmap(frame.buffer(), cw, ch, &mut self.canvas) {
                    self.frame_index += 1;
                    self.current_delay_ms = delay_ms;
                    (self.frame_index, self.current_delay_ms)
                } else {
                    self.reset_to_first()
                }
            }
            None | Some(Err(_)) => self.reset_to_first(),
        }
    }

    fn seek_to(&mut self, target_index: usize) {
        if target_index == self.frame_index {
            return;
        }
        if target_index < self.frame_index {
            self.reset_to_first();
        }
        while self.frame_index < target_index {
            let (idx, _) = self.step_next();
            if idx == 0 {
                break;
            }
        }
    }
}

#[derive(Clone)]
enum GuideMedia {
    Static(Arc<RgbaImage>),
    Animated(Arc<AnimatedGuide>),
}

impl GuideMedia {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Static(img) => (img.width(), img.height()),
            Self::Animated(anim) => (anim.width, anim.height),
        }
    }

    fn nbytes(&self) -> usize {
        match self {
            Self::Static(img) => img.as_raw().len(),
            Self::Animated(anim) => anim.nbytes(),
        }
    }

    fn is_animated(&self) -> bool {
        matches!(self, Self::Animated(_))
    }

    fn as_static(&self) -> Option<&RgbaImage> {
        match self {
            Self::Static(img) => Some(img.as_ref()),
            Self::Animated(_) => None,
        }
    }

    fn as_animated(&self) -> Option<&Arc<AnimatedGuide>> {
        match self {
            Self::Static(_) => None,
            Self::Animated(anim) => Some(anim),
        }
    }
}

struct DecodedGuideBundle {
    media: GuideMedia,
    thumb: Pixmap,
    bytes_hash: Option<u64>,
}

fn check_decode_dims(w: u32, h: u32) -> bool {
    w > 0
        && h > 0
        && w <= MAX_DECODE_DIM
        && h <= MAX_DECODE_DIM
        && (w as u64).saturating_mul(h as u64) <= MAX_DECODE_PIXELS
}

fn check_anim_dims_and_size(w: u32, h: u32, byte_len: usize) -> bool {
    check_decode_dims(w, h)
        && byte_len <= MAX_GUIDE_ANIM_BYTES
        && (w as u64).saturating_mul(h as u64) <= MAX_ANIM_FRAME_PIXELS
}

fn decode_static_guide_bundle(bytes: &[u8], bhash: Option<u64>) -> Option<DecodedGuideBundle> {
    let src = decode_image_bytes(bytes)?;
    let large = contain_guide_rgba(&src)?;
    let thumb = normalize_thumbnail_rgba(&src)?;
    Some(DecodedGuideBundle {
        media: GuideMedia::Static(Arc::new(large)),
        thumb,
        bytes_hash: bhash,
    })
}

fn build_animated_bundle_from_frames(
    raw_bytes: &[u8],
    shared: Arc<[u8]>,
    format: AnimatedFormat,
    bhash: Option<u64>,
    mut frames: image::Frames<'static>,
) -> Option<DecodedGuideBundle> {
    let f0 = frames.next()?.ok()?;
    // Probe only 1 more frame: single-frame files use the static pipeline.
    let Some(Ok(_f1)) = frames.next() else {
        drop(frames);
        drop(shared);
        return decode_static_guide_bundle(raw_bytes, bhash);
    };
    drop(frames);

    let thumb = normalize_thumbnail_rgba(f0.buffer())?;
    let first_delay_ms = normalize_frame_delay_ms(f0.delay());
    let (cw, ch) = fit_guide_max_dims(f0.buffer().width(), f0.buffer().height());
    let mut first_pm = Pixmap::new(cw, ch)?;
    if !write_rgba_to_premultiplied_pixmap(f0.buffer(), cw, ch, &mut first_pm) {
        return None;
    }
    let anim = AnimatedGuide {
        format,
        width: cw,
        height: ch,
        bytes: shared,
        first_frame: Arc::new(first_pm),
        first_delay_ms,
    };
    Some(DecodedGuideBundle {
        media: GuideMedia::Animated(Arc::new(anim)),
        thumb,
        bytes_hash: bhash,
    })
}

fn decode_guide_bundle(bytes: &[u8]) -> Option<DecodedGuideBundle> {
    if bytes.is_empty() {
        return None;
    }
    let bhash = Some(hash_bytes(bytes));
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let fmt = reader.format();

    match fmt {
        Some(ImageFormat::Gif) => {
            let probe = GifDecoder::new(Cursor::new(bytes)).ok()?;
            let (w, h) = probe.dimensions();
            if !check_anim_dims_and_size(w, h, bytes.len()) {
                return None;
            }
            drop(probe);
            let shared: Arc<[u8]> = Arc::from(bytes);
            let decoder = GifDecoder::new(Cursor::new(SharedBytes(Arc::clone(&shared)))).ok()?;
            build_animated_bundle_from_frames(
                bytes,
                shared,
                AnimatedFormat::Gif,
                bhash,
                decoder.into_frames(),
            )
        }
        Some(ImageFormat::WebP) => {
            let probe = WebPDecoder::new(Cursor::new(bytes)).ok()?;
            let (w, h) = probe.dimensions();
            if !probe.has_animation() {
                drop(probe);
                return decode_static_guide_bundle(bytes, bhash);
            }
            if !check_anim_dims_and_size(w, h, bytes.len()) {
                return None;
            }
            drop(probe);
            let shared: Arc<[u8]> = Arc::from(bytes);
            let decoder = WebPDecoder::new(Cursor::new(SharedBytes(Arc::clone(&shared)))).ok()?;
            build_animated_bundle_from_frames(
                bytes,
                shared,
                AnimatedFormat::WebP,
                bhash,
                decoder.into_frames(),
            )
        }
        _ => decode_static_guide_bundle(bytes, bhash),
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Caches (LRU + Byte-Bounded Badge Cache)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BadgeKey {
    palette: HudPalette,
    bucket: i32,
    directional: bool,
    lock_on: bool,
    behind: bool,
    badge_px: u32,
}

struct BadgeEntry {
    pixmap: Arc<Pixmap>,
    nbytes: usize,
    seq: u64,
}

struct ByteBoundedBadgeLru {
    max_bytes: usize,
    total_bytes: usize,
    seq: u64,
    evictions: u64,
    map: HashMap<BadgeKey, BadgeEntry>,
}

impl ByteBoundedBadgeLru {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            seq: 0,
            evictions: 0,
            map: HashMap::new(),
        }
    }

    fn get(&mut self, key: &BadgeKey) -> Option<Arc<Pixmap>> {
        let entry = self.map.get_mut(key)?;
        self.seq += 1;
        entry.seq = self.seq;
        Some(Arc::clone(&entry.pixmap))
    }

    fn insert(&mut self, key: BadgeKey, pixmap: Pixmap) -> Arc<Pixmap> {
        let arc = Arc::new(pixmap);
        let nbytes = arc.data().len();

        if let Some(old) = self.map.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.nbytes);
        }

        // Oversized single-entry guard: never allow cache bytes to exceed max_bytes.
        if nbytes > self.max_bytes {
            let cleared = self.map.len() as u64;
            self.map.clear();
            self.total_bytes = 0;
            self.evictions += cleared;
            return arc;
        }

        while self.total_bytes + nbytes > self.max_bytes {
            if !self.evict_one() {
                break;
            }
        }

        self.seq += 1;
        let seq = self.seq;
        self.total_bytes += nbytes;
        self.map.insert(
            key,
            BadgeEntry {
                pixmap: Arc::clone(&arc),
                nbytes,
                seq,
            },
        );
        debug_assert!(self.total_bytes <= self.max_bytes);
        arc
    }

    fn evict_one(&mut self) -> bool {
        let Some(victim_key) = self
            .map
            .iter()
            .min_by_key(|(_, entry)| entry.seq)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        if let Some(removed) = self.map.remove(&victim_key) {
            self.total_bytes = self.total_bytes.saturating_sub(removed.nbytes);
            self.evictions += 1;
            return true;
        }
        false
    }

    fn clear(&mut self) {
        self.map.clear();
        self.total_bytes = 0;
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn bytes(&self) -> usize {
        self.total_bytes
    }

    fn evictions(&self) -> u64 {
        self.evictions
    }
}

struct LruMap<K, V> {
    cap: usize,
    seq: u64,
    map: HashMap<K, (V, u64)>,
}

impl<K: Eq + Hash + Clone, V> LruMap<K, V> {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            seq: 0,
            map: HashMap::new(),
        }
    }

    fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (v, s) = self.map.get_mut(key)?;
        self.seq += 1;
        *s = self.seq;
        Some(&*v)
    }

    fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.get(key).map(|(v, _)| v)
    }

    fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.remove(key).map(|(v, _)| v)
    }

    fn insert(&mut self, key: K, val: V) {
        self.map.remove(&key);
        while self.map.len() >= self.cap {
            if !self.evict_one() {
                break;
            }
        }
        self.seq += 1;
        let seq = self.seq;
        self.map.insert(key, (val, seq));
    }

    fn evict_oldest_kv(&mut self) -> Option<(K, V)> {
        let victim = self
            .map
            .iter()
            .min_by_key(|(_, (_, s))| *s)
            .map(|(k, _)| k.clone())?;
        self.map.remove(&victim).map(|(v, _)| (victim, v))
    }

    fn evict_one(&mut self) -> bool {
        self.evict_oldest_kv().is_some()
    }

    fn clear(&mut self) {
        self.map.clear();
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RadarKey {
    Base(HudPalette, u32),
    Dot(HudPalette, u32),
}

struct CachedThumb {
    pixmap: Arc<Pixmap>,
    gen_id: u64,
    bytes_hash: Option<u64>,
}

struct CachedGuide {
    media: GuideMedia,
    gen_id: u64,
    bytes_hash: Option<u64>,
    nbytes: usize,
}

/// Shared thread-safe image cache so `put_thumbnail`/`put_guide_image` can decode on
/// background threads without blocking `HudFonts` on the UI thread.
pub struct NavHudImageCache {
    thumbs: LruMap<String, CachedThumb>,
    guides: LruMap<String, CachedGuide>,
    guide_total_bytes: usize,
    next_gen_id: u64,
}

impl Default for NavHudImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NavHudImageCache {
    pub fn new() -> Self {
        Self {
            thumbs: LruMap::new(MAX_THUMB_CACHE_ENTRIES),
            guides: LruMap::new(MAX_GUIDE_CACHE_ENTRIES),
            guide_total_bytes: 0,
            next_gen_id: 1,
        }
    }

    pub fn clear(&mut self) {
        self.thumbs.clear();
        self.guides.clear();
        self.guide_total_bytes = 0;
    }

    pub fn thumb_count(&self) -> usize {
        self.thumbs.len()
    }

    pub fn guide_count(&self) -> usize {
        self.guides.len()
    }

    pub fn guide_bytes(&self) -> usize {
        self.guide_total_bytes
    }

    fn alloc_gen_id(&mut self) -> u64 {
        let id = self.next_gen_id;
        self.next_gen_id += 1;
        id
    }

    fn has_thumb_hash(&mut self, key: &str, bytes_hash: u64) -> bool {
        let clean = key.trim();
        if clean.is_empty() {
            return false;
        }
        self.thumbs
            .get(clean)
            .is_some_and(|e| e.bytes_hash == Some(bytes_hash))
    }

    fn has_guide_hash(&mut self, key: &str, bytes_hash: u64) -> bool {
        let clean = key.trim();
        if clean.is_empty() {
            return false;
        }
        let guide_hit = self
            .guides
            .get(clean)
            .is_some_and(|e| e.bytes_hash == Some(bytes_hash));
        guide_hit && self.thumbs.get(clean).is_some()
    }

    fn put_decoded_thumbnail(
        &mut self,
        key: &str,
        pixmap: Pixmap,
        bytes_hash: Option<u64>,
    ) -> bool {
        let clean = key.trim();
        if clean.is_empty() {
            return false;
        }
        let gen_id = self.alloc_gen_id();
        self.thumbs.insert(
            clean.to_owned(),
            CachedThumb {
                pixmap: Arc::new(pixmap),
                gen_id,
                bytes_hash,
            },
        );
        true
    }

    pub fn put_thumbnail_bytes(&mut self, key: &str, bytes: &[u8]) -> bool {
        let clean = key.trim();
        if clean.is_empty() {
            return false;
        }
        let bhash = hash_bytes(bytes);
        if self.has_thumb_hash(clean, bhash) {
            return true;
        }
        let Some(src) = decode_image_bytes(bytes) else {
            return false;
        };
        self.put_thumbnail_image_inner(clean, &src, Some(bhash))
    }

    pub fn put_thumbnail_rgba(&mut self, key: &str, w: u32, h: u32, straight_rgba: &[u8]) -> bool {
        let clean = key.trim();
        if clean.is_empty() || w == 0 || h == 0 {
            return false;
        }
        let Some(expected) = (w as usize).checked_mul(h as usize).and_then(|p| p.checked_mul(4))
        else {
            return false;
        };
        if straight_rgba.len() != expected {
            return false;
        }
        let Some(src) = RgbaImage::from_raw(w, h, straight_rgba.to_vec()) else {
            return false;
        };
        self.put_thumbnail_image_inner(clean, &src, None)
    }

    fn put_thumbnail_image_inner(
        &mut self,
        clean_key: &str,
        src: &RgbaImage,
        bytes_hash: Option<u64>,
    ) -> bool {
        let Some(pm) = normalize_thumbnail_rgba(src) else {
            return false;
        };
        self.put_decoded_thumbnail(clean_key, pm, bytes_hash)
    }

    fn insert_guide_media(
        &mut self,
        clean_key: &str,
        media: GuideMedia,
        bytes_hash: Option<u64>,
    ) -> u64 {
        if let Some(old) = self.guides.remove(clean_key) {
            self.guide_total_bytes = self.guide_total_bytes.saturating_sub(old.nbytes);
        }
        let nbytes = media.nbytes();
        while (self.guides.len() >= MAX_GUIDE_CACHE_ENTRIES
            || self.guide_total_bytes.saturating_add(nbytes) > MAX_GUIDE_CACHE_BYTES)
            && !self.guides.is_empty()
        {
            if let Some((_, evicted)) = self.guides.evict_oldest_kv() {
                self.guide_total_bytes = self.guide_total_bytes.saturating_sub(evicted.nbytes);
            } else {
                break;
            }
        }
        let gen_id = self.alloc_gen_id();
        self.guide_total_bytes = self.guide_total_bytes.saturating_add(nbytes);
        self.guides.insert(
            clean_key.to_owned(),
            CachedGuide {
                media,
                gen_id,
                bytes_hash,
                nbytes,
            },
        );
        gen_id
    }

    fn insert_guide_bundle(&mut self, key: &str, bundle: DecodedGuideBundle) -> bool {
        let clean = key.trim();
        if clean.is_empty() {
            return false;
        }
        let bhash = bundle.bytes_hash;
        self.insert_guide_media(clean, bundle.media, bhash);
        self.put_decoded_thumbnail(clean, bundle.thumb, bhash)
    }

    pub fn put_guide_bytes(&mut self, key: &str, bytes: &[u8]) -> bool {
        let clean = key.trim();
        if clean.is_empty() {
            return false;
        }
        let bhash = hash_bytes(bytes);
        if self.has_guide_hash(clean, bhash) {
            return true;
        }
        let Some(bundle) = decode_guide_bundle(bytes) else {
            return false;
        };
        self.insert_guide_bundle(clean, bundle)
    }

    pub fn put_guide_rgba(&mut self, key: &str, w: u32, h: u32, straight_rgba: &[u8]) -> bool {
        let clean = key.trim();
        if clean.is_empty() || w == 0 || h == 0 {
            return false;
        }
        let Some(expected) = (w as usize).checked_mul(h as usize).and_then(|p| p.checked_mul(4))
        else {
            return false;
        };
        if straight_rgba.len() != expected {
            return false;
        }
        let Some(src) = RgbaImage::from_raw(w, h, straight_rgba.to_vec()) else {
            return false;
        };
        self.put_guide_image_inner(clean, &src, None)
    }

    fn put_guide_image_inner(
        &mut self,
        clean_key: &str,
        src: &RgbaImage,
        bytes_hash: Option<u64>,
    ) -> bool {
        let Some(large) = contain_guide_rgba(src) else {
            return false;
        };
        self.insert_guide_media(clean_key, GuideMedia::Static(Arc::new(large)), bytes_hash);
        let _ = self.put_thumbnail_image_inner(clean_key, src, bytes_hash);
        true
    }

    fn get_thumb(&mut self, key: &str) -> Option<(Arc<Pixmap>, u64)> {
        let clean = key.trim();
        if clean.is_empty() {
            return None;
        }
        self.thumbs
            .get(clean)
            .map(|e| (Arc::clone(&e.pixmap), e.gen_id))
    }

    fn get_guide(&mut self, key: &str) -> Option<(GuideMedia, u64)> {
        let clean = key.trim();
        if clean.is_empty() {
            return None;
        }
        self.guides.get(clean).map(|e| (e.media.clone(), e.gen_id))
    }

    fn get_guide_anim(&self, key: &str) -> Option<(u64, Arc<AnimatedGuide>)> {
        let clean = key.trim();
        if clean.is_empty() {
            return None;
        }
        self.guides
            .peek(clean)
            .and_then(|e| e.media.as_animated().map(|anim| (e.gen_id, Arc::clone(anim))))
    }
}

// ---------------------------------------------------------------------------
// Renderer Types & Telemetry
// ---------------------------------------------------------------------------

pub type LabelResolver = Arc<dyn Fn(i32, Option<i32>, &str) -> Option<String> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct NavRenderOptions<'a> {
    pub delta_deg: Option<f64>,
    pub lang: Option<&'a str>,
    pub scale: f32,
    pub hotkey_label: &'a str,
    pub hold_progress: f32,
    pub spot_image: Option<&'a [u8]>,
    pub distance_m: Option<f64>,
    pub guide_hotkey_label: &'a str,
    pub guide_open: bool,
    pub guide_zoomed: bool,
    pub guide_zoom_mode: GuideZoomMode,
    pub max_guide_w: Option<u32>,
    pub max_guide_h: Option<u32>,
    pub guide_status: &'a str,
    pub guide_frame_index: usize,
    /// `(index, total)` of the community guide suggestion shown for the target, if any.
    pub guide_suggestion: Option<(usize, usize)>,
    /// Display name of the "next guide suggestion" hotkey.
    pub guide_next_hotkey_label: &'a str,
    pub flash_text: &'a str,
    pub flash_kind: HudPalette,
    pub edit_mode: bool,
    pub edit_hint: &'a str,
}

impl Default for NavRenderOptions<'_> {
    fn default() -> Self {
        Self {
            delta_deg: None,
            lang: None,
            scale: 1.0,
            hotkey_label: "Alt+F11",
            hold_progress: 0.0,
            spot_image: None,
            distance_m: None,
            guide_hotkey_label: "",
            guide_open: false,
            guide_zoomed: false,
            guide_zoom_mode: GuideZoomMode::Cycle,
            max_guide_w: None,
            max_guide_h: None,
            guide_status: "",
            guide_frame_index: 0,
            guide_suggestion: None,
            guide_next_hotkey_label: "",
            flash_text: "",
            flash_kind: HudPalette::Info,
            edit_mode: false,
            edit_hint: "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HudRenderMeta {
    pub expanded: bool,
    pub has_spot_card: bool,
    pub guide_open: bool,
    pub guide_zoomed: bool,
    pub guide_animated: bool,
    pub guide_frame_index: usize,
    pub card_cache_hit: bool,
    pub badge_cache_hit: bool,
    pub badge_rect: [i32; 4],
    pub palette: HudPalette,
    pub lock_on: bool,
    pub behind: bool,
    pub radar: bool,
    pub radar_xy: Option<[i32; 2]>,
    pub angle_bucket: i32,
    pub lang: String,
    pub floor_tag: String,
    pub title: String,
    pub subtitle: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HudRenderStats {
    pub badge_cache_bytes: usize,
    pub badge_cache_count: usize,
    pub max_badge_cache_bytes: usize,
    pub card_cache_count: usize,
    pub thumb_cache_count: usize,
    pub guide_cache_count: usize,
    pub radar_cache_count: usize,
    pub radar_cache_bytes: usize,
    pub card_builds: u64,
    pub badge_builds: u64,
    pub badge_evictions: u64,
    pub text_shape_calls: u64,
    pub last_frame_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CardKey {
    title_text: String,
    dist_int: Option<i32>,
    floor_tag: String,
    state: u8,
    expanded: bool,
    lang: String,
    scale_q: i32,
    subtitle_text: String,
    hold_q: u8,
    show_spot: bool,
    thumb_token: Option<(bool, u64)>,
    spot_desc: String,
    palette: HudPalette,
    show_guide: bool,
    guide_placeholder: String,
    guide_footer: String,
    eff_guide_zoomed: bool,
    max_guide_w: Option<u32>,
    max_guide_h: Option<u32>,
}

struct CachedCard {
    pixmap: Arc<Pixmap>,
    badge_xy: [i32; 2],
    badge_px: u32,
    has_spot: bool,
    guide_rect: Option<[i32; 4]>,
}

#[inline]
const fn nav_state_code(state: NavState) -> u8 {
    match state {
        NavState::Tracking => 0,
        NavState::Arrived => 1,
        NavState::FloorMismatch => 2,
        NavState::Lost => 3,
        NavState::Idle => 4,
    }
}

// ---------------------------------------------------------------------------
// Floor Tag & Bilingual Text Formatting
// ---------------------------------------------------------------------------

/// Matches `\b(?:F_)?B(\d+)\b` (case-insensitive) without pulling in a regex crate.
fn floor_depth_rank(floor_id: Option<&str>, floor_name: &str, is_ug: bool) -> Option<i32> {
    if !is_ug {
        return Some(0);
    }
    for raw in [floor_name, floor_id.unwrap_or("")] {
        let txt = raw.trim();
        if txt.is_empty() {
            continue;
        }
        for tok in txt.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if tok.is_empty() {
                continue;
            }
            let lower = tok.to_ascii_lowercase();
            let rest = lower.strip_prefix("f_").unwrap_or(&lower);
            if let Some(digits) = rest.strip_prefix('b')
                && !digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = digits.parse::<i32>()
            {
                return Some(-n);
            }
        }
    }
    None
}

pub fn clean_floor_label(floor_id: Option<&str>, floor_name: &str) -> String {
    let name_clean = floor_name.trim();
    if !name_clean.is_empty() && !name_clean.chars().all(|c| c.is_numeric()) {
        return name_clean.to_owned();
    }
    if let Some(fid) = floor_id {
        let fid_clean = fid.trim();
        if !fid_clean.is_empty()
            && fid_clean != "0"
            && !fid_clean.chars().all(|c| c.is_numeric())
        {
            return fid_clean.to_owned();
        }
    }
    String::new()
}

pub fn format_floor_tag(guidance: &NavGuidance, lang: &str) -> String {
    if guidance.target.is_none() {
        return String::new();
    }
    if guidance.floor_mismatch {
        let p_rank = floor_depth_rank(
            guidance.player_floor_id.as_deref(),
            &guidance.player_floor_name,
            guidance.player_is_underground,
        );
        let t_rank = floor_depth_rank(
            guidance.target_floor_id.as_deref(),
            &guidance.target_floor_name,
            guidance.target_is_underground,
        );
        let arrow = if !guidance.player_is_underground && guidance.target_is_underground {
            "↓ "
        } else if guidance.player_is_underground && !guidance.target_is_underground {
            "↑ "
        } else if let (Some(pr), Some(tr)) = (p_rank, t_rank)
            && pr != tr
        {
            if tr < pr { "↓ " } else { "↑ " }
        } else {
            "↕ "
        };
        let tag = if guidance.target_is_underground {
            let mut lbl =
                clean_floor_label(guidance.target_floor_id.as_deref(), &guidance.target_floor_name);
            if lbl.is_empty() {
                lbl = t_lang("hud.floor.tag_underground", Some(lang), &[]);
            }
            format!("{arrow}{lbl}")
        } else {
            format!("{arrow}{}", t_lang("hud.floor.tag_surface", Some(lang), &[]))
        };
        if tag.chars().count() <= 12 {
            tag
        } else {
            let prefix: String = tag.chars().take(11).collect();
            format!("{prefix}…")
        }
    } else if guidance.target_is_underground {
        let mut raw_floor =
            clean_floor_label(guidance.target_floor_id.as_deref(), &guidance.target_floor_name);
        if raw_floor.is_empty() {
            raw_floor = t_lang("hud.floor.tag_underground", Some(lang), &[]);
        }
        if raw_floor.chars().count() <= 10 {
            raw_floor
        } else {
            let prefix: String = raw_floor.chars().take(9).collect();
            format!("{prefix}…")
        }
    } else {
        String::new()
    }
}

fn format_floor_human(
    floor_id: Option<&str>,
    floor_name: &str,
    is_ug: bool,
    lang: &str,
) -> String {
    if is_ug {
        let label = clean_floor_label(floor_id, floor_name);
        if !label.is_empty() {
            t_lang("hud.floor.underground", Some(lang), &[("floor", &label)])
        } else {
            t_lang("hud.floor.underground_generic", Some(lang), &[])
        }
    } else {
        t_lang("hud.floor.surface", Some(lang), &[])
    }
}

struct ResolvedTexts {
    title: String,
    subtitle: String,
    floor_tag: String,
    palette: HudPalette,
    expanded: bool,
    dist_int: Option<i32>,
    description: String,
}

fn resolve_texts_and_mode(
    guidance: &NavGuidance,
    lang: &str,
    hotkey_label: &str,
    guide_hotkey_label: &str,
    distance_m: Option<f64>,
    label_resolver: Option<&LabelResolver>,
) -> ResolvedTexts {
    let eff_dist = distance_m
        .or_else(|| guidance.distance_opt_m())
        .filter(|d| d.is_finite())
        .map(|d| d.max(0.0));
    let dist_int = eff_dist.map(|d| d.round().clamp(0.0, 999_999.0) as i32);
    let floor_tag = format_floor_tag(guidance, lang);

    let mut target_name = guidance
        .target
        .as_ref()
        .map(|t| t.display_name())
        .unwrap_or_default();

    if let (Some(target), Some(resolver)) = (&guidance.target, label_resolver)
        && let Some(label_id_i64) = target.label_id
        && let Ok(label_id) = i32::try_from(label_id_i64)
    {
        let raw_name_l = target.name.trim().to_lowercase();
        let raw_cat_l = target.category.trim().to_lowercase();
        let is_generic = raw_name_l.is_empty()
            || raw_cat_l.is_empty()
            || raw_name_l == raw_cat_l
            || raw_name_l.starts_with("pin ")
            || raw_name_l == "điểm đánh dấu"
            || raw_name_l == "map pin";
        if is_generic && let Some(localized) = resolver(target.map_id, Some(label_id), lang) {
            let trimmed = localized.trim();
            if !trimmed.is_empty() {
                target_name = trimmed.to_owned();
            }
        }
    }

    let description = guidance
        .target
        .as_ref()
        .map(|t| t.description.clone())
        .unwrap_or_default();

    let mark_hint = if !hotkey_label.is_empty() {
        t_lang("hud.hint.mark_short", Some(lang), &[("hotkey", hotkey_label)])
    } else {
        String::new()
    };
    let guide_hint = if !guide_hotkey_label.is_empty() {
        t_lang(
            "hud.hint.guide_short",
            Some(lang),
            &[("hotkey", guide_hotkey_label)],
        )
    } else {
        String::new()
    };
    let action_hint = [mark_hint.as_str(), guide_hint.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" • ");

    if guidance.state == NavState::Lost {
        if guidance.is_route_mode {
            let sec = guidance.route_section_label.trim();
            let sec_lbl = if sec.is_empty() { "S1" } else { sec };
            return ResolvedTexts {
                title: t_lang("hud.route.ready_title", Some(lang), &[("sec", sec_lbl)]),
                subtitle: t_lang("hud.route.teleport_hint", Some(lang), &[]),
                floor_tag: String::new(),
                palette: HudPalette::Info,
                expanded: true,
                dist_int: None,
                description: String::new(),
            };
        }
        return ResolvedTexts {
            title: t_lang("hud.state.lost", Some(lang), &[]),
            subtitle: t_lang("hud.hint.open_map", Some(lang), &[]),
            floor_tag: String::new(),
            palette: HudPalette::Warning,
            expanded: true,
            dist_int: None,
            description: String::new(),
        };
    }

    if guidance.state == NavState::Idle || guidance.target.is_none() {
        if guidance.is_route_mode && guidance.route_target_map_id.is_some() {
            let map_name = if guidance.route_target_name.trim().is_empty() {
                format!("Map {}", guidance.route_target_map_id.unwrap_or(2))
            } else {
                guidance.route_target_name.trim().to_owned()
            };
            return ResolvedTexts {
                title: t_lang("hud.route.wrong_map_title", Some(lang), &[("map", &map_name)]),
                subtitle: t_lang("hud.route.wrong_map_hint", Some(lang), &[]),
                floor_tag: String::new(),
                palette: HudPalette::Warning,
                expanded: true,
                dist_int: None,
                description: String::new(),
            };
        }
        let sub_hint = guidance.route_target_name.trim().to_owned();
        return ResolvedTexts {
            title: t_lang("hud.target.none", Some(lang), &[]),
            expanded: !sub_hint.is_empty(),
            subtitle: sub_hint,
            floor_tag: String::new(),
            palette: HudPalette::Info,
            dist_int: None,
            description: String::new(),
        };
    }

    let meters_str = dist_int.unwrap_or(0).to_string();
    let dist_str = t_lang("hud.distance", Some(lang), &[("meters", &meters_str)]);
    let name_part = if target_name.is_empty() {
        t_lang("hud.title", Some(lang), &[])
    } else {
        target_name
    };
    let prefix = match (
        guidance.route_section_label.trim(),
        guidance.route_step,
        guidance.route_total,
    ) {
        (sec, Some(step), Some(total)) if !sec.is_empty() => format!("[{sec} #{step}/{total}] "),
        (_, Some(step), Some(total)) => format!("#{step}/{total} "),
        _ => String::new(),
    };
    let title = format!("{prefix}{name_part} • {dist_str}");
    let stop_kind_hint = match guidance.route_stop_kind.trim() {
        "waypoint" => t_lang("hud.route.waypoint_hint", Some(lang), &[]),
        "teleport" => t_lang("hud.route.teleport_hint", Some(lang), &[]),
        _ => String::new(),
    };

    match guidance.state {
        NavState::FloorMismatch => {
            let p_floor = format_floor_human(
                guidance.player_floor_id.as_deref(),
                &guidance.player_floor_name,
                guidance.player_is_underground,
                lang,
            );
            let t_floor = format_floor_human(
                guidance.target_floor_id.as_deref(),
                &guidance.target_floor_name,
                guidance.target_is_underground,
                lang,
            );
            let mut sub = t_lang(
                "hud.state.floor_mismatch",
                Some(lang),
                &[("player_floor", &p_floor), ("target_floor", &t_floor)],
            );
            if !guidance.route_target_name.is_empty() {
                sub = format!("{sub} • {}", guidance.route_target_name);
            } else if !guide_hint.is_empty() {
                sub = format!("{sub} • {guide_hint}");
            }
            ResolvedTexts {
                title,
                subtitle: sub,
                floor_tag,
                palette: HudPalette::Warning,
                expanded: true,
                dist_int,
                description,
            }
        }
        NavState::Arrived => {
            let arrived_str = t_lang("hud.state.arrived", Some(lang), &[]);
            let tail = if !stop_kind_hint.is_empty() {
                stop_kind_hint.as_str()
            } else {
                action_hint.as_str()
            };
            let sub = if !tail.is_empty() {
                format!("{arrived_str} • {tail}")
            } else {
                arrived_str
            };
            ResolvedTexts {
                title,
                subtitle: sub,
                floor_tag,
                palette: HudPalette::Success,
                expanded: true,
                dist_int,
                description,
            }
        }
        NavState::Tracking => {
            let force_route_tp = guidance.route_stop_kind.trim() == "teleport"
                || (guidance.is_route_mode && eff_dist.is_some_and(|d| d > 220.0));
            let expanded = force_route_tp || eff_dist.is_some_and(|d| d <= COMPACT_DIST_THRESHOLD_M);
            let sub = if force_route_tp {
                t_lang("hud.route.teleport_hint", Some(lang), &[])
            } else if expanded {
                if !stop_kind_hint.is_empty() {
                    stop_kind_hint
                } else if !action_hint.is_empty() {
                    action_hint
                } else {
                    t_lang("hud.hint.mark", Some(lang), &[("hotkey", hotkey_label)])
                }
            } else {
                String::new()
            };
            ResolvedTexts {
                title,
                subtitle: sub,
                floor_tag,
                palette: HudPalette::Info,
                expanded,
                dist_int,
                description,
            }
        }
        NavState::Lost | NavState::Idle => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// NavHudRenderer (2-Layer Pre-Baked Composition)
// ---------------------------------------------------------------------------

pub struct NavHudRenderer {
    fonts: HudFonts,
    badge_cache: ByteBoundedBadgeLru,
    radar_cache: LruMap<RadarKey, Arc<Pixmap>>,
    card_cache: LruMap<CardKey, CachedCard>,
    image_cache: Arc<Mutex<NavHudImageCache>>,
    anim_player: Option<StreamingAnimPlayer>,
    label_resolver: Option<LabelResolver>,
    last_render_meta: HudRenderMeta,
    card_builds: u64,
    badge_builds: u64,
    text_shape_calls: u64,
    last_frame_ns: u64,
}

impl Default for NavHudRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl NavHudRenderer {
    pub const BADGE_STEP_DEG: i32 = BADGE_STEP_DEG;
    pub const BADGE_COUNT: i32 = BADGE_COUNT;
    pub const MAX_BADGE_CACHE_BYTES: usize = MAX_BADGE_CACHE_BYTES;
    pub const MAX_CARD_CACHE_ENTRIES: usize = MAX_CARD_CACHE_ENTRIES;
    pub const MAX_THUMB_CACHE_ENTRIES: usize = MAX_THUMB_CACHE_ENTRIES;
    pub const MAX_GUIDE_CACHE_ENTRIES: usize = MAX_GUIDE_CACHE_ENTRIES;
    pub const THUMB_SIZE: (u32, u32) = THUMB_SIZE;
    pub const GUIDE_IMAGE_MAX: (u32, u32) = GUIDE_IMAGE_MAX;
    pub const LOCK_ON_DEG: f64 = LOCK_ON_DEG;
    pub const BEHIND_DEG: f64 = BEHIND_DEG;
    pub const COMPACT_DIST_THRESHOLD_M: f64 = COMPACT_DIST_THRESHOLD_M;
    pub const RADAR_RANGE_M: f64 = RADAR_RANGE_M;

    pub fn new() -> Self {
        Self::with_image_cache(Arc::new(Mutex::new(NavHudImageCache::new())))
    }

    pub fn with_image_cache(image_cache: Arc<Mutex<NavHudImageCache>>) -> Self {
        Self {
            fonts: HudFonts::new(),
            badge_cache: ByteBoundedBadgeLru::new(MAX_BADGE_CACHE_BYTES),
            radar_cache: LruMap::new(MAX_RADAR_CACHE_ENTRIES),
            card_cache: LruMap::new(MAX_CARD_CACHE_ENTRIES),
            image_cache,
            anim_player: None,
            label_resolver: None,
            last_render_meta: HudRenderMeta::default(),
            card_builds: 0,
            badge_builds: 0,
            text_shape_calls: 0,
            last_frame_ns: 0,
        }
    }

    pub fn with_badge_cache_max_bytes(mut self, max_bytes: usize) -> Self {
        self.badge_cache = ByteBoundedBadgeLru::new(max_bytes.min(MAX_BADGE_CACHE_BYTES));
        self
    }

    pub fn image_cache_handle(&self) -> Arc<Mutex<NavHudImageCache>> {
        Arc::clone(&self.image_cache)
    }

    pub fn set_label_resolver(&mut self, resolver: Option<LabelResolver>) {
        self.label_resolver = resolver;
    }

    pub fn clear_caches(&mut self) {
        self.badge_cache.clear();
        self.radar_cache.clear();
        self.card_cache.clear();
        self.anim_player = None;
        self.image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .clear();
    }

    fn reset_anim_player(&mut self) {
        self.anim_player = None;
    }

    fn ensure_anim_player(&mut self, gen_id: u64, anim: &Arc<AnimatedGuide>) -> u32 {
        if self.anim_player.as_ref().is_none_or(|p| p.gen_id != gen_id) {
            self.anim_player = Some(StreamingAnimPlayer::new(gen_id, Arc::clone(anim)));
        }
        self.anim_player
            .as_ref()
            .map_or(anim.first_delay_ms, |p| p.current_delay_ms)
    }

    fn advance_anim_player(&mut self, gen_id: u64, anim: &Arc<AnimatedGuide>) -> (usize, u32) {
        if self.anim_player.as_ref().is_none_or(|p| p.gen_id != gen_id) {
            self.anim_player = Some(StreamingAnimPlayer::new(gen_id, Arc::clone(anim)));
            return (0, anim.first_delay_ms);
        }
        self.anim_player
            .as_mut()
            .map_or((0, anim.first_delay_ms), StreamingAnimPlayer::step_next)
    }

    pub fn put_thumbnail(&self, key: &str, bytes: &[u8]) -> bool {
        self.image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .put_thumbnail_bytes(key, bytes)
    }

    pub fn put_thumbnail_rgba(&self, key: &str, w: u32, h: u32, straight_rgba: &[u8]) -> bool {
        self.image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .put_thumbnail_rgba(key, w, h, straight_rgba)
    }

    pub fn put_guide_image(&self, key: &str, bytes: &[u8]) -> bool {
        self.image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .put_guide_bytes(key, bytes)
    }

    pub fn put_guide_image_rgba(&self, key: &str, w: u32, h: u32, straight_rgba: &[u8]) -> bool {
        self.image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .put_guide_rgba(key, w, h, straight_rgba)
    }

    pub fn last_meta(&self) -> &HudRenderMeta {
        &self.last_render_meta
    }

    pub fn stats(&self) -> HudRenderStats {
        let (thumb_cache_count, guide_cache_count) = {
            let guard = self
                .image_cache
                .lock()
                .expect("nav hud image cache lock poisoned");
            (guard.thumb_count(), guard.guide_count())
        };
        let radar_cache_bytes: usize = self
            .radar_cache
            .map
            .values()
            .map(|(pm, _)| pm.data().len())
            .sum();
        HudRenderStats {
            badge_cache_bytes: self.badge_cache.bytes(),
            badge_cache_count: self.badge_cache.len(),
            max_badge_cache_bytes: self.badge_cache.max_bytes,
            card_cache_count: self.card_cache.len(),
            thumb_cache_count,
            guide_cache_count,
            radar_cache_count: self.radar_cache.len(),
            radar_cache_bytes,
            card_builds: self.card_builds,
            badge_builds: self.badge_builds,
            badge_evictions: self.badge_cache.evictions(),
            text_shape_calls: self.text_shape_calls,
            last_frame_ns: self.last_frame_ns,
        }
    }

    pub fn quantize_angle_bucket(angle_deg: Option<f64>) -> i32 {
        let Some(a) = angle_deg.filter(|v| v.is_finite()) else {
            return -1;
        };
        let norm = a.rem_euclid(360.0);
        ((norm / BADGE_STEP_DEG as f64).round() as i32).rem_euclid(BADGE_COUNT)
    }

    fn build_compass_badge(
        palette: HudPalette,
        angle_bucket: i32,
        badge_px: u32,
        directional: bool,
        lock_on: bool,
        behind: bool,
    ) -> Option<Pixmap> {
        let eff_pal = if lock_on { HudPalette::LockOn } else { palette };
        let pal = eff_pal.colors();
        let sz = badge_px.max(24);
        let sz_f = sz as f32;
        let cx = sz_f * 0.5;
        let cy = sz_f * 0.5;
        let pad = 2.0f32;
        let r = (sz_f - pad * 2.0) * 0.5;

        let mut pm = Pixmap::new(sz, sz)?;

        // 1. Opaque-ish Midnight Slate dial base (`alpha = 244 >= 240`)
        if let Some(dial) = PathBuilder::from_circle(cx, cy, r) {
            pm.fill_path(&dial, &make_paint(DIAL_BG), FillRule::Winding, Transform::identity(), None);
        }

        // 2. Translucent accent dial glow via SourceOver
        let tint_color = if behind && !lock_on {
            [245, 158, 11, 46]
        } else {
            pal.badge_tint
        };
        let inner_r = (r - 1.5).max(1.0);
        if let Some(inner_disc) = PathBuilder::from_circle(cx, cy, inner_r) {
            pm.fill_path(
                &inner_disc,
                &make_paint(tint_color),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        // 3. Outer ring + cardinal ticks + forward 12-o'clock notch
        let ring_color = if behind && !lock_on {
            AMBER_RING
        } else {
            pal.accent
        };
        let ring_w = if lock_on { 2.4f32 } else { 1.8f32 };
        if let Some(ring) = build_circle_stroke_path(cx, cy, r, ring_w) {
            let stroke = Stroke {
                width: ring_w,
                ..Default::default()
            };
            pm.stroke_path(
                &ring,
                &make_paint(ring_color),
                &stroke,
                Transform::identity(),
                None,
            );
        }

        let tick_len = 3.0f32;
        let tick_stroke = Stroke {
            width: 1.2f32,
            line_cap: LineCap::Round,
            ..Default::default()
        };
        let tick_paint = make_paint([148, 163, 184, 150]);
        for t_deg in [90.0f32, 180.0, 270.0] {
            let (ox, oy) = rotate_point(0.0, -(r - 1.2), t_deg);
            let (ix, iy) = rotate_point(0.0, -(r - 1.2 - tick_len), t_deg);
            let mut pb = PathBuilder::new();
            pb.move_to(cx + ox, cy + oy);
            pb.line_to(cx + ix, cy + iy);
            if let Some(p) = pb.finish() {
                pm.stroke_path(&p, &tick_paint, &tick_stroke, Transform::identity(), None);
            }
        }

        let top_y = pad + 0.5;
        if lock_on && directional {
            let notch_w = (r * 0.24).max(2.0);
            let notch_h = (r * 0.22).max(2.0);
            if let Some(notch) = build_polygon_path(&[
                (cx, top_y + notch_h),
                (cx - notch_w, top_y - 0.3),
                (cx + notch_w, top_y - 0.3),
            ]) {
                pm.fill_path(
                    &notch,
                    &make_paint([255, 255, 255, 255]),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            if let Some(glow) = build_circle_stroke_path(cx, cy, (r - ring_w).max(1.0), 1.0) {
                pm.stroke_path(
                    &glow,
                    &make_paint([204, 251, 241, 120]),
                    &Stroke {
                        width: 1.0,
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
        } else {
            let notch_w = (r * 0.20).max(1.8);
            let notch_h = (r * 0.18).max(1.8);
            if let Some(notch) = build_polygon_path(&[
                (cx, top_y + notch_h),
                (cx - notch_w, top_y - 0.3),
                (cx + notch_w, top_y - 0.3),
            ]) {
                pm.fill_path(
                    &notch,
                    &make_paint([226, 232, 240, 200]),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }

        // 4. Unified symmetrical Tinh Tú (Star Dart) OR non-directional status glyph
        if directional && angle_bucket >= 0 {
            let deg = (angle_bucket * BADGE_STEP_DEG) as f32;
            let style = if lock_on {
                crate::star_dart::StarDartStyle::HUD_LOCK_ON
            } else {
                crate::star_dart::StarDartStyle::HUD
            };
            crate::star_dart::draw_star_dart(
                &mut pm,
                cx,
                cy,
                r * 0.64,
                deg.to_radians(),
                style,
                Transform::identity(),
                None,
            );
        } else if eff_pal == HudPalette::Warning {
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - r * 0.36);
            pb.line_to(cx, cy + r * 0.10);
            if let Some(p) = pb.finish() {
                pm.stroke_path(
                    &p,
                    &make_paint(pal.accent),
                    &Stroke {
                        width: 2.4,
                        line_cap: LineCap::Round,
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
            let dot_r = (r * 0.11).max(1.3);
            if let Some(dot) = PathBuilder::from_circle(cx, cy + r * 0.30, dot_r) {
                pm.fill_path(&dot, &make_paint(pal.accent), FillRule::Winding, Transform::identity(), None);
            }
        } else if eff_pal == HudPalette::Success {
            let mut pb = PathBuilder::new();
            pb.move_to(cx - r * 0.34, cy + r * 0.02);
            pb.line_to(cx - r * 0.06, cy + r * 0.30);
            pb.line_to(cx + r * 0.38, cy - r * 0.28);
            if let Some(p) = pb.finish() {
                pm.stroke_path(
                    &p,
                    &make_paint(pal.accent),
                    &Stroke {
                        width: 2.4,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
        } else {
            let dot_r = (r * 0.22).max(1.8);
            if let Some(dot) = PathBuilder::from_circle(cx, cy, dot_r) {
                pm.fill_path(&dot, &make_paint(pal.accent), FillRule::Winding, Transform::identity(), None);
            }
        }

        Some(pm)
    }

    pub fn get_compass_badge(
        &mut self,
        palette: HudPalette,
        angle_deg: Option<f64>,
        scale: f32,
        directional: bool,
        lock_on: bool,
        behind: bool,
    ) -> HudResult<(Arc<Pixmap>, bool, i32)> {
        if !scale.is_finite() {
            return Err(HudError::InvalidBuffer(format!(
                "badge scale must be finite, got {scale}"
            )));
        }
        let s = scale.clamp(0.85, 1.8);
        let badge_px = (36.0 * s).round().max(24.0) as u32;
        let bucket = if directional {
            Self::quantize_angle_bucket(angle_deg)
        } else {
            -1
        };
        let eff_pal = if lock_on { HudPalette::LockOn } else { palette };
        let key = BadgeKey {
            palette: eff_pal,
            bucket,
            directional,
            lock_on,
            behind,
            badge_px,
        };

        if let Some(hit) = self.badge_cache.get(&key) {
            return Ok((hit, true, bucket));
        }

        let badge = Self::build_compass_badge(eff_pal, bucket, badge_px, directional, lock_on, behind)
            .ok_or_else(|| HudError::Render("failed to build compass badge pixmap".into()))?;
        self.badge_builds += 1;
        let arc = self.badge_cache.insert(key, badge);
        Ok((arc, false, bucket))
    }

    fn build_radar_base(palette: HudPalette, badge_px: u32) -> Option<Pixmap> {
        let pal = palette.colors();
        let sz = badge_px.max(24);
        let sz_f = sz as f32;
        let cx = sz_f * 0.5;
        let cy = sz_f * 0.5;
        let pad = 2.0f32;
        let r = (sz_f - pad * 2.0) * 0.5;

        let mut pm = Pixmap::new(sz, sz)?;
        if let Some(dial) = PathBuilder::from_circle(cx, cy, r) {
            pm.fill_path(&dial, &make_paint(DIAL_BG), FillRule::Winding, Transform::identity(), None);
        }
        let inner_r = (r - 1.5).max(1.0);
        if let Some(disc) = PathBuilder::from_circle(cx, cy, inner_r) {
            pm.fill_path(&disc, &make_paint(pal.badge_tint), FillRule::Winding, Transform::identity(), None);
        }
        if let Some(ring) = build_circle_stroke_path(cx, cy, r, 1.8) {
            pm.stroke_path(
                &ring,
                &make_paint(pal.accent),
                &Stroke {
                    width: 1.8,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
        for frac in [0.36f32, 0.70] {
            if let Some(rr) = PathBuilder::from_circle(cx, cy, r * frac) {
                pm.stroke_path(
                    &rr,
                    &make_paint([148, 163, 184, 110]),
                    &Stroke {
                        width: 1.0,
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
        }
        let top_y = pad + 0.5;
        let notch_w = (r * 0.20).max(1.8);
        let notch_h = (r * 0.18).max(1.8);
        if let Some(notch) = build_polygon_path(&[
            (cx, top_y + notch_h),
            (cx - notch_w, top_y - 0.3),
            (cx + notch_w, top_y - 0.3),
        ]) {
            pm.fill_path(
                &notch,
                &make_paint([226, 232, 240, 200]),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        let pr = r * 0.20;
        if let Some(player) = build_polygon_path(&[
            (cx, cy - pr),
            (cx + pr * 0.78, cy + pr * 0.72),
            (cx, cy + pr * 0.30),
            (cx - pr * 0.78, cy + pr * 0.72),
        ]) {
            pm.fill_path(
                &player,
                &make_paint([226, 232, 240, 235]),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        Some(pm)
    }

    fn build_radar_dot(palette: HudPalette, badge_px: u32) -> Option<Pixmap> {
        let pal = palette.colors();
        let dot_px = ((badge_px as f32 * 0.27).round() as u32).max(7);
        let mut pm = Pixmap::new(dot_px, dot_px)?;
        let c = dot_px as f32 * 0.5;
        if let Some(outer) = PathBuilder::from_circle(c, c, c - 0.25) {
            pm.fill_path(
                &outer,
                &make_paint([255, 255, 255, 255]),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        let inner_r = (c * 0.6).max(1.0);
        if let Some(inner) = PathBuilder::from_circle(c, c, inner_r) {
            pm.fill_path(&inner, &make_paint(pal.accent), FillRule::Winding, Transform::identity(), None);
        }
        Some(pm)
    }

    fn get_radar_parts(
        &mut self,
        palette: HudPalette,
        delta_deg: f64,
        distance_m: f64,
        scale: f32,
    ) -> HudResult<(Arc<Pixmap>, Arc<Pixmap>, [i32; 2])> {
        if !scale.is_finite() {
            return Err(HudError::InvalidBuffer(format!(
                "radar scale must be finite, got {scale}"
            )));
        }
        let s = scale.clamp(0.85, 1.8);
        let badge_px = (36.0 * s).round().max(24.0) as u32;
        let base_key = RadarKey::Base(palette, badge_px);
        let dot_key = RadarKey::Dot(palette, badge_px);

        let base = if let Some(b) = self.radar_cache.get(&base_key) {
            Arc::clone(b)
        } else {
            let built = Arc::new(
                Self::build_radar_base(palette, badge_px)
                    .ok_or_else(|| HudError::Render("failed to build radar base".into()))?,
            );
            self.radar_cache.insert(base_key, Arc::clone(&built));
            built
        };

        let dot = if let Some(d) = self.radar_cache.get(&dot_key) {
            Arc::clone(d)
        } else {
            let built = Arc::new(
                Self::build_radar_dot(palette, badge_px)
                    .ok_or_else(|| HudError::Render("failed to build radar dot".into()))?,
            );
            self.radar_cache.insert(dot_key, Arc::clone(&built));
            built
        };

        let bp = badge_px as f64;
        let r = bp * 0.5 - (bp * 0.12).max(2.0);
        let safe_dist = if distance_m.is_finite() {
            distance_m.max(0.0)
        } else {
            0.0
        };
        let safe_delta = if delta_deg.is_finite() { delta_deg } else { 0.0 };
        let frac = (safe_dist / RADAR_RANGE_M).clamp(0.0, 1.0);
        let rr = r * (0.10 + 0.90 * frac);
        let rad = safe_delta.to_radians();
        let cx = bp * 0.5 + rad.sin() * rr;
        let cy = bp * 0.5 - rad.cos() * rr;
        let dw = dot.width() as i32;
        let dh = dot.height() as i32;
        let dx = ((cx - dw as f64 * 0.5).round() as i32).clamp(0, badge_px as i32 - dw);
        let dy = ((cy - dh as f64 * 0.5).round() as i32).clamp(0, badge_px as i32 - dh);
        Ok((base, dot, [dx, dy]))
    }

    pub fn get_radar_badge(
        &mut self,
        palette: HudPalette,
        delta_deg: f64,
        distance_m: f64,
        scale: f32,
    ) -> HudResult<(Pixmap, [i32; 2])> {
        let (base, dot, [dx, dy]) = self.get_radar_parts(palette, delta_deg, distance_m, scale)?;
        let mut frame = (*base).clone();
        frame.draw_pixmap(
            dx,
            dy,
            (*dot).as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        Ok((frame, [dx, dy]))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_static_card(
        &mut self,
        title_text: &str,
        subtitle_text: &str,
        floor_tag: &str,
        palette: HudPalette,
        expanded: bool,
        scale: f32,
        hold_progress: f32,
        show_spot_card: bool,
        spot_thumb: Option<&Pixmap>,
        spot_desc: &str,
        spot_large: bool,
        spot_media_large: Option<&GuideMedia>,
        spot_placeholder: &str,
        spot_footer: &str,
        guide_zoomed: bool,
        max_guide_w: Option<u32>,
        max_guide_h: Option<u32>,
    ) -> HudResult<CachedCard> {
        let s = scale.clamp(0.85, 1.8);
        let pal = palette.colors();

        let title_px = (14.0 * s).round().max(10.0);
        let sub_px = (11.5 * s).round().max(10.0);
        let tag_px = (10.5 * s).round().max(9.0);
        let desc_px = (11.5 * s).round().max(10.0);
        let guide_line_h = (16.0 * s).round().max(12.0) as i32;

        let title_style = TextStyle::new(title_px, FontWeightRole::Bold, pal.title);
        let sub_style = TextStyle::new(sub_px, FontWeightRole::Regular, pal.sub);
        let tag_style = TextStyle::new(tag_px, FontWeightRole::Bold, pal.tag_text);
        let desc_style = TextStyle::new(desc_px, FontWeightRole::Regular, [226, 232, 240, 255])
            .with_line_height(guide_line_h as f32);
        let footer_style = TextStyle::new(sub_px, FontWeightRole::Regular, [148, 163, 184, 255]);

        let pad_x = (10.0 * s).round() as i32;
        let pad_y = if expanded {
            (8.0 * s).round() as i32
        } else {
            (6.0 * s).round() as i32
        };
        let badge_px = (36.0 * s).round().max(24.0) as i32;
        let gap_badge = (10.0 * s).round() as i32;
        let gap_tag = (8.0 * s).round() as i32;
        let max_text_w = if expanded && palette == HudPalette::Warning {
            (305.0 * s).round() as i32
        } else {
            (270.0 * s).round() as i32
        };
        let min_pill_w = (212.0 * s).round() as i32;

        // Measure floor tag pill
        let mut tag_w = 0i32;
        let mut tag_h = 0i32;
        let mut tag_metrics = Default::default();
        let tag_pad_x = (6.0 * s).round() as i32;
        let tag_pad_y = (2.0 * s).round() as i32;
        if !floor_tag.is_empty() {
            self.text_shape_calls += 1;
            tag_metrics =
                self.fonts
                    .measure_text(floor_tag, tag_style, None, Wrap::None, Ellipsize::None);
            let tw = tag_metrics.width.ceil() as i32;
            let th = (tag_metrics.visual_height.max(tag_px * 0.72)).ceil() as i32;
            tag_w = tw + tag_pad_x * 2;
            tag_h = ((18.0 * s).round() as i32).max(th + tag_pad_y * 2);
        }

        let avail_title_w = ((110.0 * s).round() as i32)
            .max(max_text_w - if !floor_tag.is_empty() { tag_w + gap_tag } else { 0 });
        self.text_shape_calls += 1;
        let clean_title = self
            .fonts
            .truncate_line(title_text, title_style, avail_title_w as f32);

        let clean_sub = if expanded && !subtitle_text.is_empty() {
            self.text_shape_calls += 1;
            self.fonts
                .truncate_line(subtitle_text, sub_style, max_text_w as f32)
        } else {
            String::new()
        };

        self.text_shape_calls += 1;
        let title_probe = if clean_title.is_empty() {
            "A"
        } else {
            clean_title.as_str()
        };
        let title_metrics =
            self.fonts
                .measure_text(title_probe, title_style, None, Wrap::None, Ellipsize::None);
        let title_w = if clean_title.is_empty() {
            0
        } else {
            title_metrics.width.ceil() as i32
        };
        let title_h = (title_metrics.visual_height.max(title_px * 0.72)).ceil() as i32;

        let (sub_w, sub_h, sub_metrics, line_gap) = if !clean_sub.is_empty() {
            self.text_shape_calls += 1;
            let sm = self.fonts.measure_text(
                &clean_sub,
                sub_style,
                None,
                Wrap::None,
                Ellipsize::None,
            );
            (
                sm.width.ceil() as i32,
                (sm.visual_height.max(sub_px * 0.72)).ceil() as i32,
                sm,
                (4.0 * s).round() as i32,
            )
        } else {
            (0, 0, Default::default(), 0)
        };

        let top_row_w = title_w
            + if !floor_tag.is_empty() {
                gap_tag + tag_w
            } else {
                0
            };
        let text_block_w = top_row_w.max(sub_w);
        let text_block_h = title_h.max(tag_h) + line_gap + sub_h;

        let pill_w =
            min_pill_w.max(pad_x * 2 + badge_px + gap_badge + text_block_w + (4.0 * s).round() as i32);
        let mut pill_h = (badge_px + pad_y * 2).max(text_block_h + pad_y * 2);
        if expanded {
            pill_h = pill_h.max((62.0 * s).round() as i32);
        }

        let mut has_spot = show_spot_card && (spot_thumb.is_some() || !spot_desc.is_empty());
        let spot_gap = (6.0 * s).round() as i32;
        let mut spot_h = 0i32;
        let mut spot_lines: Vec<String> = Vec::new();
        let thumb_w = (THUMB_SIZE.0 as f32 * s).round() as i32;
        let thumb_h = (THUMB_SIZE.1 as f32 * s).round() as i32;
        let spot_pad_x = (10.0 * s).round() as i32;
        let spot_pad_y = (8.0 * s).round() as i32;
        let thumb_gap = (10.0 * s).round() as i32;

        let mut total_w = pill_w;
        let mut guide_pixmap: Option<Pixmap> = None;
        let mut guide_img_layout: Option<(i32, i32)> = None;
        let mut guide_lines: Vec<String> = Vec::new();
        let mut guide_footer = String::new();

        if spot_large {
            has_spot = true;
            let (max_gh, max_desc_lines) = if guide_zoomed {
                let w_cap = max_guide_w
                    .filter(|&w| w > 0)
                    .map(|w| w.min(MAX_GUIDE_DIM_CLAMP) as i32)
                    .unwrap_or_else(|| (760.0 * s).round() as i32);
                let h_cap = max_guide_h
                    .filter(|&h| h > 0)
                    .map(|h| h.min(MAX_GUIDE_DIM_CLAMP) as i32)
                    .unwrap_or_else(|| (520.0 * s).round() as i32);
                let zoom_w = ((620.0 * s).round() as i32)
                    .min(w_cap)
                    .max((312.0 * s).round() as i32);
                total_w = pill_w.max(zoom_w);
                let gh_cap = ((410.0 * s).round() as i32)
                    .min(h_cap)
                    .max((196.0 * s).round() as i32);
                (gh_cap, 8usize)
            } else {
                total_w = pill_w.max((312.0 * s).round() as i32);
                ((196.0 * s).round() as i32, 5usize)
            };

            let inner_w = (total_w - spot_pad_x * 2).max(20);
            if let Some(media) = spot_media_large {
                let (sw, sh) = media.dimensions();
                let sw = sw.max(1);
                let sh = sh.max(1);
                let mut gw = inner_w;
                let mut gh = ((sh as f64) * (gw as f64 / sw as f64)).round() as i32;
                if gh > max_gh {
                    gh = max_gh;
                    gw = ((sw as f64) * (gh as f64 / sh as f64)).round() as i32;
                }
                let gw_u = gw.max(1) as u32;
                let gh_u = gh.max(1) as u32;
                guide_img_layout = Some((gw_u as i32, gh_u as i32));
                if let Some(src_large) = media.as_static() {
                    let resized = if gw_u == sw && gh_u == sh {
                        src_large.clone()
                    } else {
                        image::imageops::resize(src_large, gw_u, gh_u, FilterType::Lanczos3)
                    };
                    guide_pixmap = rgba_image_to_premultiplied_pixmap(&resized);
                }
            }

            let body = if !spot_desc.is_empty() {
                spot_desc
            } else {
                spot_placeholder
            };
            if !body.is_empty() {
                self.text_shape_calls += 1;
                guide_lines = self
                    .fonts
                    .wrap_lines(body, desc_style, inner_w as f32, max_desc_lines);
            }
            if !spot_footer.is_empty() {
                self.text_shape_calls += 1;
                guide_footer = self.fonts.truncate_line(spot_footer, sub_style, inner_w as f32);
            }

            let reserved_img_dim = guide_pixmap
                .as_ref()
                .map(|gp| (gp.width() as i32, gp.height() as i32))
                .or_else(|| {
                    if spot_media_large.is_some_and(GuideMedia::is_animated) {
                        guide_img_layout
                    } else {
                        None
                    }
                });

            let mut content_h = 0i32;
            if let Some((_, gh_i)) = reserved_img_dim {
                content_h += gh_i;
            }
            if !guide_lines.is_empty() {
                if reserved_img_dim.is_some() {
                    content_h += (8.0 * s).round() as i32;
                }
                content_h += (guide_lines.len() as i32) * guide_line_h;
            }
            if !guide_footer.is_empty() {
                content_h += (8.0 * s).round() as i32 + guide_line_h;
            }
            spot_h = ((40.0 * s).round() as i32).max(content_h + spot_pad_y * 2);
        } else if has_spot {
            total_w = pill_w.max((272.0 * s).round() as i32);
            let desc_avail_w = ((110.0 * s).round() as i32).max(
                total_w
                    - spot_pad_x * 2
                    - if spot_thumb.is_some() {
                        thumb_w + thumb_gap
                    } else {
                        0
                    },
            );
            if !spot_desc.is_empty() {
                self.text_shape_calls += 1;
                spot_lines = self
                    .fonts
                    .wrap_lines(spot_desc, desc_style, desc_avail_w as f32, 2);
            }
            let desc_h = (spot_lines.len() as i32) * guide_line_h
                + if spot_lines.len() > 1 {
                    (2.0 * s).round() as i32
                } else {
                    0
                };
            let content_h = desc_h.max(if spot_thumb.is_some() { thumb_h } else { 0 });
            spot_h = content_h + spot_pad_y * 2;
        }

        let total_h = (pill_h + if has_spot { spot_gap + spot_h } else { 0 })
            .clamp(1, MAX_GUIDE_DIM_CLAMP as i32);
        let total_w = total_w.clamp(1, MAX_GUIDE_DIM_CLAMP as i32);

        let mut pixmap = Pixmap::new(total_w as u32, total_h as u32).ok_or_else(|| {
            HudError::Render(format!(
                "failed to allocate card pixmap {total_w}x{total_h}"
            ))
        })?;

        let pill_radius = if !expanded {
            pill_h as f32 * 0.5
        } else {
            (14.0 * s).round()
        };
        let border_w = (1.5 * s).max(1.0);
        if let Some(pill_path) = build_rounded_rect_path(
            0.75,
            0.75,
            total_w as f32 - 1.5,
            pill_h as f32 - 1.5,
            pill_radius,
        ) {
            pixmap.fill_path(
                &pill_path,
                &make_paint(GLASS_BG),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            pixmap.stroke_path(
                &pill_path,
                &make_paint(pal.border),
                &Stroke {
                    width: border_w,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
            if let Some(inner_pill) = build_rounded_rect_path(
                3.0,
                3.0,
                (total_w as f32 - 6.0).max(4.0),
                (pill_h as f32 - 6.0).max(4.0),
                (pill_radius - 2.0).max(2.0),
            ) {
                pixmap.stroke_path(
                    &inner_pill,
                    &make_paint([211, 188, 142, 46]),
                    &Stroke {
                        width: (0.9 * s).max(0.7),
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
        }

        if has_spot {
            let sy0 = (pill_h + spot_gap) as f32 + 0.5;
            let sh = (total_h as f32 - 1.0 - sy0).max(2.0);
            let spot_radius = (12.0 * s).round();
            if let Some(spot_path) =
                build_rounded_rect_path(0.75, sy0, total_w as f32 - 1.5, sh, spot_radius)
            {
                pixmap.fill_path(
                    &spot_path,
                    &make_paint(GLASS_BG),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
                pixmap.stroke_path(
                    &spot_path,
                    &make_paint(SPOT_BORDER),
                    &Stroke {
                        width: (1.2 * s).max(1.0),
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
                if let Some(inner_spot) = build_rounded_rect_path(
                    3.0,
                    sy0 + 2.25,
                    (total_w as f32 - 6.0).max(4.0),
                    (sh - 4.5).max(4.0),
                    (spot_radius - 2.0).max(2.0),
                ) {
                    pixmap.stroke_path(
                        &inner_spot,
                        &make_paint([211, 188, 142, 42]),
                        &Stroke {
                            width: (0.9 * s).max(0.7),
                            ..Default::default()
                        },
                        Transform::identity(),
                        None,
                    );
                }
            }
        }

        // Floor tag pill
        let text_x = pad_x + badge_px + gap_badge;
        let text_top = (pill_h - text_block_h) / 2;
        let tag_x = text_x + title_w + gap_tag;
        let tag_y = text_top + ((title_h.max(tag_h) - tag_h) / 2).max(0);
        if !floor_tag.is_empty()
            && let Some(tag_path) = build_rounded_rect_path(
                tag_x as f32,
                tag_y as f32,
                tag_w as f32,
                tag_h as f32,
                tag_h as f32 * 0.5,
            )
        {
            pixmap.fill_path(
                &tag_path,
                &make_paint(pal.tag_bg),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            pixmap.stroke_path(
                &tag_path,
                &make_paint(pal.border),
                &Stroke {
                    width: (1.0 * s).max(1.0),
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }

        // Hold progress bar
        if hold_progress > 0.0 && hold_progress < 1.0 {
            let bar_margin = (14.0 * s).round();
            let bar_h = (2.5 * s).round().max(1.5);
            let bar_y0 = pill_h as f32 - bar_h - (2.0 * s).round().max(1.5);
            let full_bar_w = (total_w as f32 - bar_margin * 2.0).max(5.0);
            let fill_w = (full_bar_w * hold_progress).max(2.0);
            if let Some(track) =
                build_rounded_rect_path(bar_margin, bar_y0, full_bar_w, bar_h, bar_h * 0.5)
            {
                pixmap.fill_path(
                    &track,
                    &make_paint([148, 163, 184, 70]),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            if let Some(fill) =
                build_rounded_rect_path(bar_margin, bar_y0, fill_w, bar_h, bar_h * 0.5)
            {
                pixmap.fill_path(
                    &fill,
                    &make_paint(pal.accent),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }

        // Composite Guide / Spot images BEFORE text
        let mut guide_rect: Option<[i32; 4]> = None;
        if spot_large {
            let spot_top = pill_h + spot_gap;
            let mut cur_y = spot_top + spot_pad_y;
            if let Some(gp) = &guide_pixmap {
                let gx = (total_w - gp.width() as i32) / 2;
                pixmap.draw_pixmap(
                    gx,
                    cur_y,
                    gp.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                cur_y += gp.height() as i32 + (8.0 * s).round() as i32;
            } else if spot_media_large.is_some_and(GuideMedia::is_animated)
                && let Some((gw_i, gh_i)) = guide_img_layout
            {
                let gx = (total_w - gw_i) / 2;
                guide_rect = Some([gx, cur_y, gw_i, gh_i]);
                cur_y += gh_i + (8.0 * s).round() as i32;
            }
            let mut pm_mut = pixmap.as_mut();
            for line in &guide_lines {
                let lm = self.fonts.measure_text(
                    line,
                    desc_style,
                    None,
                    Wrap::None,
                    Ellipsize::None,
                );
                self.fonts.draw_text(
                    &mut pm_mut,
                    line,
                    spot_pad_x as f32,
                    cur_y as f32 - lm.visual_top_offset,
                    None,
                    TextAlign::Left,
                    Wrap::None,
                    Ellipsize::None,
                    desc_style,
                    TextEffect::None,
                );
                cur_y += guide_line_h;
            }
            if !guide_footer.is_empty() {
                if !guide_lines.is_empty() {
                    cur_y += (8.0 * s).round() as i32;
                }
                let fm = self.fonts.measure_text(
                    &guide_footer,
                    footer_style,
                    None,
                    Wrap::None,
                    Ellipsize::None,
                );
                self.fonts.draw_text(
                    &mut pm_mut,
                    &guide_footer,
                    spot_pad_x as f32,
                    cur_y as f32 - fm.visual_top_offset,
                    None,
                    TextAlign::Left,
                    Wrap::None,
                    Ellipsize::None,
                    footer_style,
                    TextEffect::None,
                );
            }
        } else if has_spot {
            let spot_top = pill_h + spot_gap;
            let mut cur_x = spot_pad_x;
            if let Some(thumb) = spot_thumb {
                let thumb_y = spot_top + (spot_h - thumb_h) / 2;
                let paint = PixmapPaint {
                    quality: FilterQuality::Bilinear,
                    ..Default::default()
                };
                let sx = thumb_w as f32 / thumb.width().max(1) as f32;
                let sy = thumb_h as f32 / thumb.height().max(1) as f32;
                pixmap.draw_pixmap(
                    0,
                    0,
                    thumb.as_ref(),
                    &paint,
                    Transform::from_scale(sx, sy).post_translate(cur_x as f32, thumb_y as f32),
                    None,
                );
                cur_x += thumb_w + thumb_gap;
            }
            if !spot_lines.is_empty() {
                let block_h = (spot_lines.len() as i32) * guide_line_h;
                let desc_y = spot_top + spot_pad_y.max((spot_h - block_h) / 2);
                let mut pm_mut = pixmap.as_mut();
                for (idx, line) in spot_lines.iter().enumerate() {
                    let lm = self.fonts.measure_text(
                        line,
                        desc_style,
                        None,
                        Wrap::None,
                        Ellipsize::None,
                    );
                    self.fonts.draw_text(
                        &mut pm_mut,
                        line,
                        cur_x as f32,
                        (desc_y + (idx as i32) * guide_line_h) as f32 - lm.visual_top_offset,
                        None,
                        TextAlign::Left,
                        Wrap::None,
                        Ellipsize::None,
                        desc_style,
                        TextEffect::None,
                    );
                }
            }
        }

        // Pill text overlay (drawn last on Layer 1)
        let mut pm_mut = pixmap.as_mut();
        let title_y = text_top + ((title_h.max(tag_h) - title_h) / 2).max(0);
        if !clean_title.is_empty() {
            self.fonts.draw_text(
                &mut pm_mut,
                &clean_title,
                text_x as f32,
                title_y as f32 - title_metrics.visual_top_offset,
                None,
                TextAlign::Left,
                Wrap::None,
                Ellipsize::None,
                title_style,
                TextEffect::None,
            );
        }
        if !floor_tag.is_empty() {
            let tw_actual = tag_metrics.width;
            let th_actual = tag_metrics.visual_height.max(tag_px * 0.72);
            let tx = tag_x as f32 + (tag_w as f32 - tw_actual) * 0.5;
            let ty = tag_y as f32 + (tag_h as f32 - th_actual) * 0.5 - tag_metrics.visual_top_offset;
            self.fonts.draw_text(
                &mut pm_mut,
                floor_tag,
                tx,
                ty,
                None,
                TextAlign::Left,
                Wrap::None,
                Ellipsize::None,
                tag_style,
                TextEffect::None,
            );
        }
        if !clean_sub.is_empty() {
            let sub_y = text_top + title_h.max(tag_h) + line_gap;
            self.fonts.draw_text(
                &mut pm_mut,
                &clean_sub,
                text_x as f32,
                sub_y as f32 - sub_metrics.visual_top_offset,
                None,
                TextAlign::Left,
                Wrap::None,
                Ellipsize::None,
                sub_style,
                TextEffect::None,
            );
        }

        let badge_xy = [pad_x, (pill_h - badge_px) / 2];
        self.card_builds += 1;
        Ok(CachedCard {
            pixmap: Arc::new(pixmap),
            badge_xy,
            badge_px: badge_px as u32,
            has_spot,
            guide_rect,
        })
    }

    pub fn render(&mut self, guidance: &NavGuidance, opts: &NavRenderOptions<'_>) -> HudResult<Pixmap> {
        let mut buf = Vec::new();
        let (w, h) = self.render_into(guidance, opts, &mut buf)?;
        let mut pm = Pixmap::new(w, h)
            .ok_or_else(|| HudError::Render(format!("failed to allocate output pixmap {w}x{h}")))?;
        pm.data_mut().copy_from_slice(&buf);
        Ok(pm)
    }

    pub fn render_into(
        &mut self,
        guidance: &NavGuidance,
        opts: &NavRenderOptions<'_>,
        out_pm_rgba: &mut Vec<u8>,
    ) -> HudResult<(u32, u32)> {
        let t_start = Instant::now();
        if !opts.scale.is_finite() {
            return Err(HudError::InvalidBuffer(format!(
                "HUD scale must be finite, got {}",
                opts.scale
            )));
        }

        let eff_lang = opts
            .lang
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(get_language);

        let eff_delta = opts
            .delta_deg
            .or(guidance.delta_deg)
            .filter(|d| d.is_finite());
        let eff_dist = opts
            .distance_m
            .or_else(|| guidance.distance_opt_m())
            .filter(|d| d.is_finite())
            .map(|d| d.max(0.0));

        let s = opts.scale.clamp(0.85, 1.8);
        let scale_q = (s * 100.0).round() as i32;
        let scale_f = scale_q as f32 / 100.0;

        let hold_clamped = if opts.hold_progress.is_finite() {
            opts.hold_progress.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let hold_q = (hold_clamped * 20.0).round() as u8;

        let resolved = resolve_texts_and_mode(
            guidance,
            &eff_lang,
            opts.hotkey_label,
            opts.guide_hotkey_label,
            eff_dist,
            self.label_resolver.as_ref(),
        );

        let base_state_palette = resolved.palette;
        let mut title_text = resolved.title;
        let mut subtitle_text = resolved.subtitle;
        let floor_tag = resolved.floor_tag;
        let mut palette = resolved.palette;
        let mut expanded = resolved.expanded;
        let dist_int = resolved.dist_int;
        let spot_desc = resolved.description;

        if !opts.flash_text.is_empty() {
            subtitle_text = opts.flash_text.to_owned();
            expanded = true;
            palette = opts.flash_kind;
        }
        if opts.edit_mode {
            subtitle_text = if !opts.edit_hint.is_empty() {
                opts.edit_hint.to_owned()
            } else {
                t_lang("hud.edit.hint", Some(&eff_lang), &[("hotkey", "")])
            };
            expanded = true;
            palette = HudPalette::Edit;
            if guidance.target.is_none() {
                title_text = t_lang("hud.edit.title", Some(&eff_lang), &[]);
            }
        }

        let target = guidance.target.as_ref();
        let has_target =
            target.is_some() && !matches!(guidance.state, NavState::Lost | NavState::Idle);
        let show_guide = opts.guide_open && has_target;
        let show_spot = guidance.show_spot_card && target.is_some() && !show_guide;

        let mut spot_thumb: Option<Arc<Pixmap>> = None;
        let mut guide_media: Option<GuideMedia> = None;
        let mut thumb_token: Option<(bool, u64)> = None;

        if let Some(t) = target {
            if show_spot {
                let img_key = if !t.image_url.is_empty() {
                    t.image_url.clone()
                } else {
                    format!("pin:{}", t.id)
                };
                let mut cache = self
                    .image_cache
                    .lock()
                    .expect("nav hud image cache lock poisoned");
                if let Some(bytes) = opts.spot_image {
                    cache.put_thumbnail_bytes(&img_key, bytes);
                }
                if let Some((pm, gen_id)) = cache.get_thumb(&img_key) {
                    spot_thumb = Some(pm);
                    thumb_token = Some((false, gen_id));
                }
            } else if show_guide && !t.image_url.is_empty() {
                let mut cache = self
                    .image_cache
                    .lock()
                    .expect("nav hud image cache lock poisoned");
                if let Some((media, gen_id)) = cache.get_guide(&t.image_url) {
                    guide_media = Some(media);
                    thumb_token = Some((true, gen_id));
                }
            }
        }

        let mut guide_placeholder = String::new();
        let mut guide_footer = String::new();
        let eff_guide_zoomed = show_guide && opts.guide_zoomed;
        if show_guide && let Some(t) = target {
            if t.description.is_empty() && guide_media.is_none() {
                let source = [("source", guide_source_name(&t.id))];
                if opts.guide_status == "error" {
                    guide_placeholder = t_lang("hud.guide.image_failed", Some(&eff_lang), &source);
                } else if opts.guide_status == "loading"
                    || (!t.image_url.is_empty() && opts.guide_status.is_empty())
                {
                    guide_placeholder = t_lang("hud.guide.loading", Some(&eff_lang), &source);
                } else {
                    guide_placeholder = t_lang("hud.guide.empty", Some(&eff_lang), &source);
                }
            }
            if !opts.guide_hotkey_label.is_empty() {
                guide_footer = if opts.guide_zoom_mode == GuideZoomMode::Cycle && !eff_guide_zoomed
                {
                    t_lang(
                        "hud.guide.zoom_hint",
                        Some(&eff_lang),
                        &[("hotkey", opts.guide_hotkey_label)],
                    )
                } else {
                    t_lang(
                        "hud.guide.close_hint",
                        Some(&eff_lang),
                        &[("hotkey", opts.guide_hotkey_label)],
                    )
                };
            }
            // Several community suggestions: say which one this is and how to see the next.
            // It goes first because the footer is cut to one line.
            if let Some((index, total)) = opts.guide_suggestion
                && total > 1
                && !opts.guide_next_hotkey_label.is_empty()
            {
                let hint = t_lang(
                    "hud.guide.suggestion_hint",
                    Some(&eff_lang),
                    &[
                        ("index", &index.to_string()),
                        ("total", &total.to_string()),
                        ("hotkey", opts.guide_next_hotkey_label),
                    ],
                );
                guide_footer = if guide_footer.is_empty() {
                    hint
                } else {
                    format!("{hint} • {guide_footer}")
                };
            }
        }

        let san_guide_w = opts
            .max_guide_w
            .filter(|&w| w > 0)
            .map(|w| w.min(MAX_GUIDE_DIM_CLAMP));
        let san_guide_h = opts
            .max_guide_h
            .filter(|&h| h > 0)
            .map(|h| h.min(MAX_GUIDE_DIM_CLAMP));
        let spot_desc_eff = if show_spot || show_guide {
            spot_desc.clone()
        } else {
            String::new()
        };

        let card_key = CardKey {
            title_text: title_text.clone(),
            dist_int,
            floor_tag: floor_tag.clone(),
            state: nav_state_code(guidance.state),
            expanded,
            lang: eff_lang.clone(),
            scale_q,
            subtitle_text: subtitle_text.clone(),
            hold_q,
            show_spot,
            thumb_token,
            spot_desc: spot_desc_eff.clone(),
            palette,
            show_guide,
            guide_placeholder: guide_placeholder.clone(),
            guide_footer: guide_footer.clone(),
            eff_guide_zoomed,
            max_guide_w: san_guide_w,
            max_guide_h: san_guide_h,
        };

        let (base_card, badge_xy, badge_px, has_spot, guide_rect, card_hit) =
            if let Some(hit) = self.card_cache.get(&card_key) {
                (
                    Arc::clone(&hit.pixmap),
                    hit.badge_xy,
                    hit.badge_px,
                    hit.has_spot,
                    hit.guide_rect,
                    true,
                )
            } else {
                let built = self.build_static_card(
                    &title_text,
                    &subtitle_text,
                    &floor_tag,
                    palette,
                    expanded,
                    scale_f,
                    hold_q as f32 / 20.0,
                    show_spot,
                    spot_thumb.as_deref(),
                    &spot_desc_eff,
                    show_guide,
                    guide_media.as_ref(),
                    &guide_placeholder,
                    &guide_footer,
                    eff_guide_zoomed,
                    san_guide_w,
                    san_guide_h,
                )?;
                let res = (
                    Arc::clone(&built.pixmap),
                    built.badge_xy,
                    built.badge_px,
                    built.has_spot,
                    built.guide_rect,
                    false,
                );
                self.card_cache.insert(card_key, built);
                res
            };

        let radar = matches!(guidance.state, NavState::Tracking | NavState::Arrived)
            && eff_delta.is_some()
            && eff_dist.is_some_and(|d| d <= RADAR_RANGE_M);
        let badge_pal = if opts.edit_mode {
            HudPalette::Edit
        } else {
            base_state_palette
        };

        let w = base_card.width();
        let h = base_card.height();
        out_pm_rgba.resize(base_card.data().len(), 0);
        out_pm_rgba.copy_from_slice(base_card.data());
        let mut frame = PixmapMut::from_bytes(out_pm_rgba, w, h)
            .ok_or_else(|| HudError::InvalidBuffer("failed to wrap output RGBA buffer".into()))?;

        let mut lock_on = false;
        let mut behind = false;
        let mut angle_bucket = -1i32;
        let mut badge_hit = true;
        let mut radar_xy = None;

        if radar {
            let (rbase, rdot, [dx, dy]) = self.get_radar_parts(
                badge_pal,
                eff_delta.unwrap_or(0.0),
                eff_dist.unwrap_or(0.0),
                scale_f,
            )?;
            frame.draw_pixmap(
                badge_xy[0],
                badge_xy[1],
                (*rbase).as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            frame.draw_pixmap(
                badge_xy[0] + dx,
                badge_xy[1] + dy,
                (*rdot).as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            radar_xy = Some([dx, dy]);
        } else {
            let directional = matches!(
                guidance.state,
                NavState::Tracking | NavState::FloorMismatch | NavState::Arrived | NavState::Idle
            ) && eff_delta.is_some();
            lock_on = directional
                && !opts.edit_mode
                && guidance.state == NavState::Tracking
                && eff_delta.is_some_and(|d| d.abs() <= LOCK_ON_DEG);
            behind = directional
                && !opts.edit_mode
                && matches!(guidance.state, NavState::Tracking | NavState::FloorMismatch)
                && eff_delta.is_some_and(|d| d.abs() > BEHIND_DEG);
            let (badge, hit, bucket) = self.get_compass_badge(
                badge_pal,
                eff_delta,
                scale_f,
                directional,
                lock_on,
                behind,
            )?;
            badge_hit = hit;
            angle_bucket = bucket;
            frame.draw_pixmap(
                badge_xy[0],
                badge_xy[1],
                (*badge).as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }

        let mut eff_guide_animated = false;
        let mut eff_guide_frame_index = 0usize;
        if show_guide
            && let Some([gx, gy, gw, gh]) = guide_rect
            && let Some(GuideMedia::Animated(anim)) = &guide_media
            && let Some((_, gid)) = thumb_token
        {
            self.ensure_anim_player(gid, anim);
            if let Some(player) = self.anim_player.as_mut() {
                if player.frame_index != opts.guide_frame_index {
                    player.seek_to(opts.guide_frame_index);
                }
                let canvas = &player.canvas;
                let gw_u = gw.max(1) as u32;
                let gh_u = gh.max(1) as u32;
                let cw = canvas.width().max(1);
                let ch = canvas.height().max(1);
                if cw == gw_u && ch == gh_u {
                    frame.draw_pixmap(
                        gx,
                        gy,
                        canvas.as_ref(),
                        &PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                } else {
                    let paint = PixmapPaint {
                        quality: FilterQuality::Bilinear,
                        ..Default::default()
                    };
                    let sx = gw_u as f32 / cw as f32;
                    let sy = gh_u as f32 / ch as f32;
                    frame.draw_pixmap(
                        0,
                        0,
                        canvas.as_ref(),
                        &paint,
                        Transform::from_scale(sx, sy).post_translate(gx as f32, gy as f32),
                        None,
                    );
                }
                eff_guide_animated = true;
                eff_guide_frame_index = player.frame_index;
            }
        }

        self.last_frame_ns = t_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.last_render_meta = HudRenderMeta {
            expanded,
            has_spot_card: has_spot,
            guide_open: show_guide,
            guide_zoomed: eff_guide_zoomed,
            guide_animated: eff_guide_animated,
            guide_frame_index: eff_guide_frame_index,
            card_cache_hit: card_hit,
            badge_cache_hit: badge_hit,
            badge_rect: [badge_xy[0], badge_xy[1], badge_px as i32, badge_px as i32],
            palette: if lock_on { HudPalette::LockOn } else { palette },
            lock_on,
            behind,
            radar,
            radar_xy,
            angle_bucket,
            lang: eff_lang,
            floor_tag,
            title: title_text,
            subtitle: subtitle_text,
        };

        Ok((w, h))
    }
}

// ---------------------------------------------------------------------------
// Positioning & Normalization Pure Functions
// ---------------------------------------------------------------------------

pub fn norm_position_mode(mode: Option<&str>) -> PositionMode {
    mode.map(PositionMode::from_str_lossy).unwrap_or_default()
}

pub fn norm_guide_zoom_mode(mode: Option<&str>) -> GuideZoomMode {
    mode.map(GuideZoomMode::from_str_lossy).unwrap_or_default()
}

pub fn norm_custom_pos(pos: Option<[f64; 2]>) -> Option<[f64; 2]> {
    let [fx, fy] = pos?;
    if !fx.is_finite() || !fy.is_finite() {
        return None;
    }
    Some([fx.clamp(0.0, 0.98), fy.clamp(0.0, 0.98)])
}

pub fn compute_hud_position(
    hud_w: i32,
    hud_h: i32,
    window_rect: Option<RectI32>,
    position_mode: PositionMode,
    gamepad_mode: bool,
    work_area: Option<(i32, i32, i32, i32)>,
    custom_pos: Option<[f64; 2]>,
) -> (i32, i32) {
    let hw = hud_w.max(1);
    let hh = hud_h.max(1);

    let (win_left, win_top, win_w, win_h) = match window_rect {
        Some(r) if r.width > 200 && r.height > 200 => (r.x, r.y, r.width, r.height),
        _ => {
            let (sw, sh) = WindowFinder::get_screen_dimensions();
            (0, 0, if sw > 0 { sw } else { 1920 }, if sh > 0 { sh } else { 1080 })
        }
    };

    let (wa_left, wa_top, wa_right, wa_bottom) = match work_area {
        Some((l, t, r, b)) if r > l && b > t => (l, t, r, b),
        _ => crate::layered::query_monitor_work_area(win_left + win_w / 2, win_top + win_h / 2),
    };

    let cpos = norm_custom_pos(custom_pos);
    let mode = if position_mode == PositionMode::Custom && cpos.is_none() {
        PositionMode::UnderMinimap
    } else {
        position_mode
    };

    let (raw_x, raw_y) = match mode {
        PositionMode::TopCenter => (
            win_left as f64 + (win_w - hw) as f64 * 0.5,
            win_top as f64 + (win_h as f64 * 0.028).round().max(18.0),
        ),
        PositionMode::BottomCenter => (
            win_left as f64 + (win_w - hw) as f64 * 0.5,
            win_top as f64 + (win_h as f64) * 0.80 - hh as f64,
        ),
        PositionMode::Custom => {
            let [fx, fy] = cpos.expect("checked above");
            (
                win_left as f64 + fx * win_w as f64,
                win_top as f64 + fy * win_h as f64,
            )
        }
        PositionMode::RightOfMinimap | PositionMode::UnderMinimap => {
            if let Ok(layout) = compute_ui_layout(win_w as u32, win_h as u32, gamepad_mode) {
                let mm = layout.minimap_rect;
                let gap = (10.0 * layout.scale_ui).round().max(8.0);
                if mode == PositionMode::RightOfMinimap {
                    (
                        win_left as f64 + (mm.x + mm.width) as f64 + gap,
                        win_top as f64 + mm.y as f64 + (mm.height as f64 * 0.08).round().max(0.0),
                    )
                } else {
                    let mm_cx = win_left as f64 + mm.x as f64 + mm.width as f64 * 0.5;
                    (
                        mm_cx - hw as f64 * 0.5,
                        win_top as f64 + (mm.y + mm.height) as f64 + gap,
                    )
                }
            } else {
                (win_left as f64 + 16.0, win_top as f64 + 16.0)
            }
        }
    };

    let margin = 8;
    let min_x = wa_left + margin;
    let max_x = (wa_right - hw - margin).max(min_x);
    let min_y = wa_top + margin;
    let max_y = (wa_bottom - hh - margin).max(min_y);

    let clamped_x = (raw_x.round() as i32).clamp(min_x, max_x);
    let clamped_y = (raw_y.round() as i32).clamp(min_y, max_y);
    (clamped_x, clamped_y)
}

pub fn finish_drag_to_custom(
    hud_rect: RectI32,
    win_rect: Option<RectI32>,
    screen_dims: (i32, i32),
) -> Option<[f64; 2]> {
    let (wl, wt, ww, wh) = match win_rect {
        Some(r) if r.width > 200 && r.height > 200 => (r.x, r.y, r.width, r.height),
        _ => (0, 0, screen_dims.0.max(1), screen_dims.1.max(1)),
    };
    let fx = (hud_rect.x - wl) as f64 / ww.max(1) as f64;
    let fy = (hud_rect.y - wt) as f64 / wh.max(1) as f64;
    norm_custom_pos(Some([fx, fy]))
}

// ---------------------------------------------------------------------------
// NavMiniHud (Win32 Layered Window Host Wrapper)
// ---------------------------------------------------------------------------

pub type PositionChangedCallback = Arc<dyn Fn(PositionMode, Option<[f64; 2]>) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct NavHudConfig {
    pub position_mode: PositionMode,
    pub custom_pos: Option<[f64; 2]>,
    pub gamepad_mode: bool,
    pub lang: Option<String>,
    pub hotkey_label: String,
    pub guide_hotkey_label: String,
    pub layout_hotkey_label: String,
    pub capture_visible: bool,
    pub guide_zoom_mode: GuideZoomMode,
    pub screen_offset: (i32, i32),
}

impl Default for NavHudConfig {
    fn default() -> Self {
        Self {
            position_mode: PositionMode::UnderMinimap,
            custom_pos: None,
            gamepad_mode: false,
            lang: None,
            hotkey_label: "Alt+F11".to_owned(),
            guide_hotkey_label: String::new(),
            layout_hotkey_label: String::new(),
            capture_visible: false,
            guide_zoom_mode: GuideZoomMode::Cycle,
            screen_offset: (0, 0),
        }
    }
}

#[derive(Debug, Clone)]
struct FlashState {
    text: String,
    kind: HudPalette,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VisualKey {
    Radar(i32, i32),
    Needle(i32),
}

#[derive(Debug, Clone, PartialEq)]
struct UploadedSig {
    state_version: u64,
    visual_key: VisualKey,
    dist_round: Option<i32>,
    win_rect: Option<RectI32>,
    work_area: Option<(i32, i32, i32, i32)>,
    pos_mode: PositionMode,
    custom_pos: Option<[f64; 2]>,
    gamepad_mode: bool,
    dragging: bool,
    guide_open: bool,
    guide_zoomed: bool,
    guide_zoom_mode: GuideZoomMode,
    guide_frame_index: usize,
}

struct NavHudShared {
    position_mode: PositionMode,
    custom_pos: Option<[f64; 2]>,
    gamepad_mode: bool,
    lang: Option<String>,
    hotkey_label: String,
    guide_hotkey_label: String,
    layout_hotkey_label: String,
    window_rect: Option<RectI32>,
    work_area: Option<(i32, i32, i32, i32)>,
    guidance: Option<NavGuidance>,
    hold_progress: f32,
    spot_image: Option<Arc<Vec<u8>>>,
    want_visible: bool,
    state_version: u64,
    force_invalidate: bool,
    guide_open: bool,
    guide_zoomed: bool,
    guide_zoom_mode: GuideZoomMode,
    guide_status: String,
    /// `(pin_id, index, total)` of the community guide suggestion currently shown.
    guide_suggestion: Option<(String, usize, usize)>,
    guide_next_hotkey_label: String,
    flash: Option<FlashState>,
    edit_mode: bool,
    edit_started: Instant,
    auto_fade_enabled: bool,
    closed: bool,
    on_position_changed: Option<PositionChangedCallback>,
    label_resolver: Option<LabelResolver>,
    resolver_version: u64,
    last_render_meta: HudRenderMeta,
    render_stats: HudRenderStats,
    render_seq: u64,
    hide_ack_seq: u64,
}

struct NavHandler {
    shared: Arc<Mutex<NavHudShared>>,
    render_cv: Arc<Condvar>,
    renderer: NavHudRenderer,
    smoother: SpringAngleSmoother,
    distance_smoother: ScalarSmoother,
    hover_fade: HoverFadeController,
    last_hud_screen_rect: Option<RectI32>,
    last_target_id: Option<String>,
    render_timer_active: bool,
    last_uploaded_sig: Option<UploadedSig>,
    applied_resolver_version: u64,
    anim_guide_gen: Option<u64>,
    anim_frame_index: usize,
    anim_next_due: Instant,
    anim_timer_active: bool,
    frame_pm_rgba: Vec<u8>,
    origin: Instant,
}

impl NavHandler {
    fn cursor_over_hud(&self) -> bool {
        let Some(rect) = self.last_hud_screen_rect else {
            return false;
        };
        let mut pt = POINT::default();
        // SAFETY: `GetCursorPos` writes cursor screen coordinates into a valid stack `POINT`.
        let ok = unsafe { GetCursorPos(&mut pt).is_ok() };
        ok && point_in_rect(pt.x, pt.y, rect)
    }

    fn update_hover_fade(
        &mut self,
        ctx: &LayeredWindowCtx<'_>,
        suppressed: bool,
        allow_step: bool,
    ) -> bool {
        if suppressed || !self.hover_fade.is_enabled() {
            return self.hover_fade.tick(false, true).is_some();
        }
        if !allow_step {
            return false;
        }
        let hovered = ctx.is_visible() && self.cursor_over_hud();
        self.hover_fade.tick(hovered, false).is_some()
    }

    fn stop_guide_anim(&mut self, ctx: &LayeredWindowCtx<'_>) {
        if self.anim_timer_active {
            ctx.kill_timer(TIMER_GUIDE_ANIM);
            self.anim_timer_active = false;
        }
        self.anim_guide_gen = None;
        self.anim_frame_index = 0;
        self.renderer.reset_anim_player();
    }

    fn arm_guide_anim_timer(&mut self, ctx: &LayeredWindowCtx<'_>, delay_ms: u32) {
        let clamped = delay_ms.clamp(10, MAX_ANIM_FRAME_DELAY_MS);
        ctx.set_timer(TIMER_GUIDE_ANIM, clamped);
        self.anim_timer_active = true;
    }

    fn sync_guide_anim(
        &mut self,
        ctx: &LayeredWindowCtx<'_>,
        now: Instant,
        guide_open: bool,
        guidance: &NavGuidance,
    ) -> usize {
        let anim_opt = if guide_open
            && let Some(target) = guidance.target.as_ref()
            && !matches!(guidance.state, NavState::Lost | NavState::Idle)
            && !target.image_url.is_empty()
        {
            let cache = self
                .renderer
                .image_cache
                .lock()
                .expect("nav hud image cache lock poisoned");
            cache.get_guide_anim(&target.image_url)
        } else {
            None
        };

        let Some((gid, anim)) = anim_opt else {
            self.stop_guide_anim(ctx);
            return 0;
        };

        if self.anim_guide_gen != Some(gid) {
            let first_delay_ms = self.renderer.ensure_anim_player(gid, &anim);
            self.anim_guide_gen = Some(gid);
            self.anim_frame_index = 0;
            self.anim_next_due = now + std::time::Duration::from_millis(u64::from(first_delay_ms));
            self.arm_guide_anim_timer(ctx, first_delay_ms);
            return 0;
        }

        let tolerance = std::time::Duration::from_millis(2);
        if now + tolerance >= self.anim_next_due {
            let overdue = now.saturating_duration_since(self.anim_next_due);
            if overdue > std::time::Duration::from_millis(300) {
                let (idx, d) = self.renderer.advance_anim_player(gid, &anim);
                self.anim_frame_index = idx;
                self.anim_next_due = now + std::time::Duration::from_millis(u64::from(d));
            } else {
                let mut next_delay_ms = anim.first_delay_ms;
                let mut steps = 0usize;
                while now + tolerance >= self.anim_next_due && steps < 2 {
                    let (idx, d) = self.renderer.advance_anim_player(gid, &anim);
                    self.anim_frame_index = idx;
                    next_delay_ms = d;
                    self.anim_next_due += std::time::Duration::from_millis(u64::from(d));
                    steps += 1;
                }
                if now + tolerance >= self.anim_next_due {
                    self.anim_next_due =
                        now + std::time::Duration::from_millis(u64::from(next_delay_ms));
                }
            }
            let remain_ms = self
                .anim_next_due
                .saturating_duration_since(now)
                .as_millis()
                .min(u128::from(MAX_ANIM_FRAME_DELAY_MS)) as u32;
            self.arm_guide_anim_timer(ctx, remain_ms);
        } else if !self.anim_timer_active {
            let remain_ms = self
                .anim_next_due
                .saturating_duration_since(now)
                .as_millis()
                .min(u128::from(MAX_ANIM_FRAME_DELAY_MS)) as u32;
            self.arm_guide_anim_timer(ctx, remain_ms);
        }

        self.anim_frame_index
    }

    fn render_tick(&mut self, ctx: &mut LayeredWindowCtx<'_>, force: bool, from_render_timer: bool) {
        let now = Instant::now();
        let now_s = self.origin.elapsed().as_secs_f64();

        let (
            want_visible,
            guidance_opt,
            win_rect,
            work_area,
            pos_mode,
            custom_pos,
            gp_mode,
            lang,
            hotkey_label,
            guide_label,
            layout_label,
            hold_progress,
            spot_image,
            mut state_ver,
            flash,
            guide_open,
            mut guide_zoomed,
            guide_zoom_mode,
            guide_status,
            guide_suggestion_raw,
            guide_next_label,
            edit_mode,
            auto_fade_enabled,
            force_inv,
            resolver_update,
        ) = {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            if let Some(fl) = &g.flash
                && now >= fl.expires_at
            {
                g.flash = None;
                g.state_version += 1;
            }
            if g.edit_mode && g.edit_started.elapsed().as_secs_f64() > EDIT_TIMEOUT_S {
                g.edit_mode = false;
                g.state_version += 1;
            }
            let force_inv = g.force_invalidate;
            g.force_invalidate = false;
            let res_upd = if g.resolver_version != self.applied_resolver_version {
                Some((g.label_resolver.clone(), g.resolver_version))
            } else {
                None
            };
            (
                g.want_visible,
                g.guidance.clone(),
                g.window_rect,
                g.work_area,
                g.position_mode,
                g.custom_pos,
                g.gamepad_mode,
                g.lang.clone(),
                g.hotkey_label.clone(),
                g.guide_hotkey_label.clone(),
                g.layout_hotkey_label.clone(),
                g.hold_progress,
                g.spot_image.clone(),
                g.state_version,
                g.flash.clone(),
                g.guide_open,
                g.guide_zoomed,
                g.guide_zoom_mode,
                g.guide_status.clone(),
                g.guide_suggestion.clone(),
                g.guide_next_hotkey_label.clone(),
                g.edit_mode,
                g.auto_fade_enabled,
                force_inv,
                res_upd,
            )
        };

        self.hover_fade.set_enabled(auto_fade_enabled);

        if let Some((res, ver)) = resolver_update {
            self.renderer.set_label_resolver(res);
            self.applied_resolver_version = ver;
        }

        if ctx.is_edit_mode() != edit_mode {
            ctx.set_edit_mode(edit_mode);
        }

        if !edit_mode && (!want_visible || guidance_opt.is_none()) {
            if ctx.is_visible() {
                ctx.hide();
                self.last_uploaded_sig = None;
            }
            self.hover_fade.reset();
            self.last_hud_screen_rect = None;
            self.stop_guide_anim(ctx);
            if self.render_timer_active {
                ctx.kill_timer(TIMER_RENDER);
                self.render_timer_active = false;
            }
            return;
        }

        let guidance = match guidance_opt {
            Some(g) if want_visible || !edit_mode => g,
            _ => NavGuidance::default(),
        };

        if !self.render_timer_active {
            ctx.set_timer(TIMER_RENDER, RENDER_TICK_MS);
            self.render_timer_active = true;
        }

        let target_id = guidance.target.as_ref().map(|t| t.id.clone());
        if target_id != self.last_target_id {
            self.distance_smoother.reset();
            self.last_target_id = target_id;
            self.stop_guide_anim(ctx);
            if guide_zoomed {
                let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
                g.guide_zoomed = false;
                g.state_version += 1;
                state_ver = g.state_version;
                guide_zoomed = false;
            }
        }

        let raw_dist = guidance.distance_opt_m();
        let smoothed_dist = if raw_dist.is_none() {
            None
        } else {
            self.distance_smoother.update_at(raw_dist, now_s)
        };

        let smoothed_delta = if guidance.delta_deg.is_some()
            && matches!(
                guidance.state,
                NavState::Tracking | NavState::FloorMismatch | NavState::Arrived | NavState::Idle
            ) {
            self.smoother.set_distance(smoothed_dist);
            self.smoother.set_target(guidance.delta_deg);
            self.smoother.step_at(now_s)
        } else {
            self.smoother.reset();
            None
        };

        let radar = smoothed_delta.is_some()
            && smoothed_dist.is_some_and(|d| d <= RADAR_RANGE_M)
            && matches!(guidance.state, NavState::Tracking | NavState::Arrived);
        let visual_key = if radar {
            VisualKey::Radar(
                smoothed_delta.unwrap_or(0.0).round() as i32,
                (smoothed_dist.unwrap_or(0.0) * 2.0).round() as i32,
            )
        } else {
            VisualKey::Needle(NavHudRenderer::quantize_angle_bucket(smoothed_delta))
        };

        let (sw, sh) = WindowFinder::get_screen_dimensions();
        let win_w = match win_rect {
            Some(r) if r.width > 200 => r.width,
            _ => if sw > 0 { sw } else { 1920 },
        };
        let win_h = match win_rect {
            Some(r) if r.height > 200 => r.height,
            _ => if sh > 0 { sh } else { 1080 },
        };
        let scale = ((win_h as f32) / 1080.0).clamp(0.85, 1.6);
        let max_guide_w = ((win_w as f64 * 0.70) as u32).max(340);
        let max_guide_h = ((win_h as f64 * 0.60) as u32).max(240);

        let guide_frame_index = self.sync_guide_anim(ctx, now, guide_open, &guidance);

        let sig = UploadedSig {
            state_version: state_ver,
            visual_key,
            dist_round: smoothed_dist.map(|d| d.round() as i32),
            win_rect,
            work_area,
            pos_mode,
            custom_pos,
            gamepad_mode: gp_mode,
            dragging: ctx.is_dragging(),
            guide_open,
            guide_zoomed,
            guide_zoom_mode,
            guide_frame_index,
        };

        let fade_suppressed = edit_mode || guide_open || ctx.is_dragging();

        if !force && !force_inv && ctx.is_visible() && self.last_uploaded_sig.as_ref() == Some(&sig)
        {
            if self.update_hover_fade(ctx, fade_suppressed, from_render_timer) {
                let _ = ctx.present_alpha(self.hover_fade.current_alpha());
            }
            return;
        }

        let edit_hint = if edit_mode {
            t_lang(
                "hud.edit.hint",
                lang.as_deref(),
                &[("hotkey", layout_label.as_str())],
            )
        } else {
            String::new()
        };

        let opts = NavRenderOptions {
            delta_deg: smoothed_delta,
            lang: lang.as_deref(),
            scale,
            hotkey_label: &hotkey_label,
            hold_progress,
            spot_image: spot_image.as_deref().map(|v| v.as_slice()),
            distance_m: smoothed_dist,
            guide_hotkey_label: &guide_label,
            guide_open,
            guide_zoomed,
            guide_zoom_mode,
            max_guide_w: Some(max_guide_w),
            max_guide_h: Some(max_guide_h),
            guide_status: &guide_status,
            guide_frame_index,
            guide_suggestion: guide_suggestion_for_target(guide_suggestion_raw.as_ref(), &guidance),
            guide_next_hotkey_label: &guide_next_label,
            flash_text: flash.as_ref().map(|f| f.text.as_str()).unwrap_or(""),
            flash_kind: flash.as_ref().map(|f| f.kind).unwrap_or(HudPalette::Info),
            edit_mode,
            edit_hint: &edit_hint,
        };

        let Ok((w, h)) = self
            .renderer
            .render_into(&guidance, &opts, &mut self.frame_pm_rgba)
        else {
            return;
        };
        let (x, y) = compute_hud_position(
            w as i32,
            h as i32,
            win_rect,
            pos_mode,
            gp_mode,
            work_area,
            custom_pos,
        );
        let _ = self.update_hover_fade(ctx, fade_suppressed, from_render_timer);
        let global_alpha = self.hover_fade.current_alpha();
        if ctx
            .present_premultiplied_rgba(
                x,
                y,
                w as i32,
                h as i32,
                &self.frame_pm_rgba,
                global_alpha,
                ctx.is_dragging(),
            )
            .is_ok()
        {
            if !ctx.is_dragging() {
                let (ox, oy) = ctx.screen_offset();
                self.last_hud_screen_rect = Some(RectI32 {
                    x: x.saturating_add(ox),
                    y: y.saturating_add(oy),
                    width: w as i32,
                    height: h as i32,
                });
            }
            self.last_uploaded_sig = Some(sig);
            let meta = self.renderer.last_meta().clone();
            let stats = self.renderer.stats();
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.last_render_meta = meta;
            g.render_stats = stats;
            g.render_seq = g.render_seq.wrapping_add(1);
            drop(g);
            self.render_cv.notify_all();
        }
    }
}

impl LayeredHandler for NavHandler {
    fn on_app_message(
        &mut self,
        ctx: &mut LayeredWindowCtx<'_>,
        msg: u32,
        _wparam: usize,
        _lparam: isize,
    ) -> Option<isize> {
        match msg {
            WM_APP_UPDATE => {
                self.render_tick(ctx, true, false);
                Some(0)
            }
            WM_APP_EDIT => {
                let edit = self
                    .shared
                    .lock()
                    .expect("nav hud shared lock poisoned")
                    .edit_mode;
                ctx.set_edit_mode(edit);
                self.render_tick(ctx, true, false);
                let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
                g.render_seq = g.render_seq.wrapping_add(1);
                drop(g);
                self.render_cv.notify_all();
                Some(0)
            }
            WM_APP_HIDE => {
                if ctx.is_edit_mode() {
                    let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
                    g.hide_ack_seq = g.hide_ack_seq.wrapping_add(1);
                    g.render_seq = g.render_seq.wrapping_add(1);
                    drop(g);
                    self.render_cv.notify_all();
                    return Some(0);
                }
                if self.render_timer_active {
                    ctx.kill_timer(TIMER_RENDER);
                    self.render_timer_active = false;
                }
                self.stop_guide_anim(ctx);
                if ctx.is_visible() {
                    ctx.hide();
                }
                self.hover_fade.reset();
                self.last_hud_screen_rect = None;
                self.last_uploaded_sig = None;
                let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
                g.hide_ack_seq = g.hide_ack_seq.wrapping_add(1);
                g.render_seq = g.render_seq.wrapping_add(1);
                drop(g);
                self.render_cv.notify_all();
                Some(0)
            }
            WM_APP_CLOSE => {
                if self.render_timer_active {
                    ctx.kill_timer(TIMER_RENDER);
                    self.render_timer_active = false;
                }
                self.stop_guide_anim(ctx);
                // SAFETY: Valid HWND on owning UI thread.
                unsafe {
                    let _ = DestroyWindow(ctx.hwnd());
                }
                Some(0)
            }
            _ => None,
        }
    }

    fn on_timer(&mut self, ctx: &mut LayeredWindowCtx<'_>, timer_id: usize) {
        if timer_id == TIMER_RENDER {
            self.render_tick(ctx, false, true);
        } else if timer_id == TIMER_GUIDE_ANIM {
            self.render_tick(ctx, false, false);
        }
    }

    fn on_drag_end(&mut self, ctx: &mut LayeredWindowCtx<'_>, window_rect: RectI32) {
        let (ox, oy) = ctx.screen_offset();
        let adj = RectI32 {
            x: window_rect.x - ox,
            y: window_rect.y - oy,
            width: window_rect.width,
            height: window_rect.height,
        };
        let screen_dims = WindowFinder::get_screen_dimensions();
        let (win_rect, cb) = {
            let g = self.shared.lock().expect("nav hud shared lock poisoned");
            (g.window_rect, g.on_position_changed.clone())
        };
        let pos = finish_drag_to_custom(adj, win_rect, screen_dims);
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.position_mode = PositionMode::Custom;
            g.custom_pos = pos;
            g.state_version += 1;
        }
        if let Some(cb) = cb {
            let _ = thread::Builder::new()
                .name("GH_NavHudPosCb".to_owned())
                .spawn(move || cb(PositionMode::Custom, pos));
        }
        self.render_tick(ctx, true, false);
    }

    fn on_destroy(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        if self.render_timer_active {
            ctx.kill_timer(TIMER_RENDER);
            self.render_timer_active = false;
        }
        self.stop_guide_anim(ctx);
    }
}

/// Click-through, non-activating, per-pixel alpha Win32 layered Navigation Mini-HUD.
pub struct NavMiniHud {
    host: LayeredWindowHost,
    shared: Arc<Mutex<NavHudShared>>,
    render_cv: Arc<Condvar>,
    image_cache: Arc<Mutex<NavHudImageCache>>,
}

impl Default for NavMiniHud {
    fn default() -> Self {
        Self::new(NavHudConfig::default())
    }
}

impl NavMiniHud {
    pub fn new(config: NavHudConfig) -> Self {
        let host = LayeredWindowHost::new(LayeredWindowConfig {
            class_prefix: "GenshinHelperNavMiniHud",
            window_title: "GenshinHelperNavMiniHUD",
            thread_name: "GH_NavMiniHud",
            screen_offset: config.screen_offset,
            enable_wda: true,
            initial_capture_visible: config.capture_visible,
            wda_refresh_ms: WDA_REFRESH_MS,
            ..LayeredWindowConfig::default()
        });
        let shared = Arc::new(Mutex::new(NavHudShared {
            position_mode: config.position_mode,
            custom_pos: norm_custom_pos(config.custom_pos),
            gamepad_mode: config.gamepad_mode,
            lang: config.lang,
            hotkey_label: if config.hotkey_label.is_empty() {
                "Alt+F11".to_owned()
            } else {
                config.hotkey_label
            },
            guide_hotkey_label: config.guide_hotkey_label,
            layout_hotkey_label: config.layout_hotkey_label,
            window_rect: None,
            work_area: None,
            guidance: None,
            hold_progress: 0.0,
            spot_image: None,
            want_visible: false,
            state_version: 0,
            force_invalidate: false,
            guide_open: false,
            guide_zoomed: false,
            guide_zoom_mode: config.guide_zoom_mode,
            guide_status: String::new(),
            guide_suggestion: None,
            guide_next_hotkey_label: String::new(),
            flash: None,
            edit_mode: false,
            edit_started: Instant::now(),
            auto_fade_enabled: true,
            closed: false,
            on_position_changed: None,
            label_resolver: None,
            resolver_version: 0,
            last_render_meta: HudRenderMeta::default(),
            render_stats: HudRenderStats::default(),
            render_seq: 0,
            hide_ack_seq: 0,
        }));
        Self {
            host,
            shared,
            render_cv: Arc::new(Condvar::new()),
            image_cache: Arc::new(Mutex::new(NavHudImageCache::new())),
        }
    }

    pub fn with_position_callback(self, cb: PositionChangedCallback) -> Self {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .on_position_changed = Some(cb);
        self
    }

    pub fn with_label_resolver(self, resolver: LabelResolver) -> Self {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.label_resolver = Some(resolver);
            g.resolver_version += 1;
        }
        self
    }

    pub fn set_label_resolver(&self, resolver: Option<LabelResolver>) {
        let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
        g.label_resolver = resolver;
        g.resolver_version += 1;
        g.force_invalidate = true;
        drop(g);
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn set_auto_fade_enabled(&self, enabled: bool) {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.auto_fade_enabled = enabled;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn is_auto_fade_enabled(&self) -> bool {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .auto_fade_enabled
    }

    fn ensure_started(&self) -> Option<isize> {
        if self
            .shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .closed
        {
            return None;
        }
        let shared = Arc::clone(&self.shared);
        let render_cv = Arc::clone(&self.render_cv);
        let image_cache = Arc::clone(&self.image_cache);
        self.host.ensure_started(move || NavHandler {
            shared,
            render_cv,
            renderer: NavHudRenderer::with_image_cache(image_cache),
            smoother: SpringAngleSmoother::default(),
            distance_smoother: ScalarSmoother::default(),
            hover_fade: HoverFadeController::default(),
            last_hud_screen_rect: None,
            last_target_id: None,
            render_timer_active: false,
            last_uploaded_sig: None,
            applied_resolver_version: 0,
            anim_guide_gen: None,
            anim_frame_index: 0,
            anim_next_due: Instant::now(),
            anim_timer_active: false,
            frame_pm_rgba: Vec::new(),
            origin: Instant::now(),
        })
    }

    pub fn compute_hud_position(
        hud_w: i32,
        hud_h: i32,
        window_rect: Option<RectI32>,
        position_mode: PositionMode,
        gamepad_mode: bool,
        work_area: Option<(i32, i32, i32, i32)>,
        custom_pos: Option<[f64; 2]>,
    ) -> (i32, i32) {
        compute_hud_position(
            hud_w,
            hud_h,
            window_rect,
            position_mode,
            gamepad_mode,
            work_area,
            custom_pos,
        )
    }

    pub fn norm_position_mode(mode: Option<&str>) -> PositionMode {
        norm_position_mode(mode)
    }

    pub fn norm_guide_zoom_mode(mode: Option<&str>) -> GuideZoomMode {
        norm_guide_zoom_mode(mode)
    }

    pub fn norm_custom_pos(pos: Option<[f64; 2]>) -> Option<[f64; 2]> {
        norm_custom_pos(pos)
    }

    pub fn hwnd(&self) -> Option<isize> {
        self.host.raw_hwnd()
    }

    pub fn is_thread_running(&self) -> bool {
        self.host.is_thread_running()
    }

    pub fn is_visible(&self) -> bool {
        self.host.is_visible()
    }

    pub fn is_edit_mode(&self) -> bool {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .edit_mode
    }

    pub fn wda_refresh_count(&self) -> usize {
        self.host.wda_refresh_count()
    }

    pub fn surface_realloc_count(&self) -> usize {
        self.host.surface_realloc_count()
    }

    pub fn position_mode(&self) -> PositionMode {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .position_mode
    }

    pub fn custom_pos(&self) -> Option<[f64; 2]> {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .custom_pos
    }

    pub fn capture_visible(&self) -> bool {
        self.host.capture_visible()
    }

    pub fn set_capture_visible(&self, visible: bool) -> bool {
        self.host.set_capture_visible(visible);
        self.host.capture_visible()
    }

    pub fn is_guide_open(&self) -> bool {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .guide_open
    }

    pub fn is_guide_zoomed(&self) -> bool {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .guide_zoomed
    }

    pub fn guide_zoom_mode(&self) -> GuideZoomMode {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .guide_zoom_mode
    }

    pub fn last_render_meta(&self) -> HudRenderMeta {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .last_render_meta
            .clone()
    }

    /// Waits on UI-thread render notifications until `pred` matches `last_render_meta`
    /// or `timeout` elapses.
    pub fn wait_for_render_meta<F>(&self, timeout: Duration, mut pred: F) -> bool
    where
        F: FnMut(&HudRenderMeta) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut guard = self.shared.lock().expect("nav hud shared lock poisoned");
        while !pred(&guard.last_render_meta) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next_guard, _) = self
                .render_cv
                .wait_timeout(guard, remaining)
                .expect("nav hud render condvar poisoned");
            guard = next_guard;
        }
        true
    }

    /// Waits on UI-thread render/hide/edit notifications until `pred()` returns `true`
    /// or `timeout` elapses.
    pub fn wait_until_render<F>(&self, timeout: Duration, mut pred: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let seq = self
                .shared
                .lock()
                .expect("nav hud shared lock poisoned")
                .render_seq;
            if pred() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let guard = self.shared.lock().expect("nav hud shared lock poisoned");
            if guard.render_seq != seq {
                continue;
            }
            let _ = self
                .render_cv
                .wait_timeout(guard, remaining)
                .expect("nav hud render condvar poisoned");
        }
    }

    #[cfg(test)]
    fn hide_ack_seq(&self) -> u64 {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .hide_ack_seq
    }

    pub fn render_stats(&self) -> HudRenderStats {
        self.shared
            .lock()
            .expect("nav hud shared lock poisoned")
            .render_stats
    }

    pub fn set_guide_zoom_mode(&self, mode: &str) -> GuideZoomMode {
        let norm = norm_guide_zoom_mode(Some(mode));
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.guide_zoom_mode = norm;
            if norm == GuideZoomMode::Compact {
                g.guide_zoomed = false;
            } else if norm == GuideZoomMode::Large && g.guide_open {
                g.guide_zoomed = true;
            }
            g.state_version += 1;
            g.force_invalidate = true;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
        norm
    }

    pub fn set_position_mode(&self, mode: &str, custom_pos: Option<[f64; 2]>) {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.position_mode = norm_position_mode(Some(mode));
            if custom_pos.is_some() {
                g.custom_pos = norm_custom_pos(custom_pos);
            }
            g.state_version += 1;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn set_custom_position(&self, custom_pos: Option<[f64; 2]>) {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.custom_pos = norm_custom_pos(custom_pos);
            g.state_version += 1;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn set_gamepad_mode(&self, enabled: bool) {
        let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
        g.gamepad_mode = enabled;
        g.state_version += 1;
    }

    pub fn set_language(&self, lang: Option<&str>) {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.lang = lang.map(str::to_owned);
            g.state_version += 1;
            g.force_invalidate = true;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn invalidate_render(&self) {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.state_version += 1;
            g.force_invalidate = true;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn set_hotkey_labels(
        &self,
        mark: Option<&str>,
        guide: Option<&str>,
        layout: Option<&str>,
    ) {
        let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
        if let Some(m) = mark {
            g.hotkey_label = m.to_owned();
        }
        if let Some(gu) = guide {
            g.guide_hotkey_label = gu.to_owned();
        }
        if let Some(la) = layout {
            g.layout_hotkey_label = la.to_owned();
        }
        g.state_version += 1;
    }

    pub fn put_thumbnail(&self, key: &str, bytes: &[u8]) -> bool {
        let clean = key.trim();
        if clean.is_empty() || bytes.is_empty() {
            return false;
        }
        let bhash = hash_bytes(bytes);
        {
            let mut cache = self
                .image_cache
                .lock()
                .expect("nav hud image cache lock poisoned");
            if cache.has_thumb_hash(clean, bhash) {
                return true;
            }
        }
        let Some(src) = decode_image_bytes(bytes) else {
            return false;
        };
        let Some(pm) = normalize_thumbnail_rgba(&src) else {
            return false;
        };
        let ok = self
            .image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .put_decoded_thumbnail(clean, pm, Some(bhash));
        if ok {
            self.shared
                .lock()
                .expect("nav hud shared lock poisoned")
                .state_version += 1;
        }
        ok
    }

    pub fn put_guide_image(&self, key: &str, bytes: &[u8]) -> bool {
        let clean = key.trim();
        if clean.is_empty() || bytes.is_empty() {
            return false;
        }
        let bhash = hash_bytes(bytes);
        {
            let mut cache = self
                .image_cache
                .lock()
                .expect("nav hud image cache lock poisoned");
            if cache.has_guide_hash(clean, bhash) {
                return true;
            }
        }
        let Some(bundle) = decode_guide_bundle(bytes) else {
            return false;
        };
        let ok = self
            .image_cache
            .lock()
            .expect("nav hud image cache lock poisoned")
            .insert_guide_bundle(clean, bundle);
        if ok {
            self.shared
                .lock()
                .expect("nav hud shared lock poisoned")
                .state_version += 1;
            let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
        }
        ok
    }

    pub fn flash(&self, text: &str, kind: HudPalette, duration_s: f64) {
        let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let dur_s = if duration_s.is_finite() {
            duration_s.max(0.5)
        } else {
            2.6
        };
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            if !clean.is_empty() {
                g.flash = Some(FlashState {
                    text: clean,
                    kind,
                    expires_at: Instant::now() + std::time::Duration::from_secs_f64(dur_s),
                });
            } else {
                g.flash = None;
            }
            g.state_version += 1;
        }
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
    }

    pub fn set_guide_open(&self, opened: bool, zoomed: Option<bool>) -> bool {
        let res = {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.guide_open = opened;
            if !g.guide_open {
                g.guide_zoomed = false;
            } else if let Some(z) = zoomed {
                g.guide_zoomed = z;
            } else if g.guide_zoom_mode == GuideZoomMode::Large {
                g.guide_zoomed = true;
            } else if g.guide_zoom_mode == GuideZoomMode::Compact {
                g.guide_zoomed = false;
            }
            g.state_version += 1;
            g.force_invalidate = true;
            g.guide_open
        };
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
        res
    }

    pub fn toggle_guide(&self) -> bool {
        let opened = {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            match g.guide_zoom_mode {
                GuideZoomMode::Cycle => {
                    if !g.guide_open {
                        g.guide_open = true;
                        g.guide_zoomed = false;
                    } else if !g.guide_zoomed {
                        g.guide_zoomed = true;
                    } else {
                        g.guide_open = false;
                        g.guide_zoomed = false;
                    }
                }
                GuideZoomMode::Large => {
                    if !g.guide_open {
                        g.guide_open = true;
                        g.guide_zoomed = true;
                    } else {
                        g.guide_open = false;
                        g.guide_zoomed = false;
                    }
                }
                GuideZoomMode::Compact => {
                    g.guide_open = !g.guide_open;
                    g.guide_zoomed = false;
                }
            }
            g.state_version += 1;
            g.force_invalidate = true;
            g.guide_open
        };
        let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
        opened
    }

    /// Shows "suggestion `index`/`total`" in the guide footer while `pin_id` is the target.
    /// `None` clears it.
    pub fn set_guide_suggestion(&self, suggestion: Option<(&str, usize, usize)>) {
        let next = suggestion.map(|(pid, i, n)| (pid.to_owned(), i, n));
        let changed = {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            if g.guide_suggestion != next {
                g.guide_suggestion = next;
                g.state_version += 1;
                true
            } else {
                false
            }
        };
        if changed {
            let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
        }
    }

    pub fn set_guide_next_hotkey_label(&self, label: &str) {
        let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
        if g.guide_next_hotkey_label != label {
            g.guide_next_hotkey_label = label.to_owned();
            g.state_version += 1;
        }
    }

    pub fn set_guide_status(&self, status: &str) {
        let changed = {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            if g.guide_status != status {
                g.guide_status = status.to_owned();
                g.state_version += 1;
                true
            } else {
                false
            }
        };
        if changed {
            let _ = self.host.post_message(WM_APP_UPDATE, 0, 0);
        }
    }

    pub fn set_edit_mode(&self, enabled: bool) -> bool {
        if enabled {
            let _ = self.ensure_started();
        }
        let state = {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.edit_mode = enabled;
            g.edit_started = Instant::now();
            g.state_version += 1;
            g.edit_mode
        };
        let _ = self.host.post_message(WM_APP_EDIT, 0, 0);
        state
    }

    pub fn toggle_edit_mode(&self) -> bool {
        let cur = self.is_edit_mode();
        self.set_edit_mode(!cur)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        guidance: NavGuidance,
        window_rect: Option<RectI32>,
        work_area: Option<(i32, i32, i32, i32)>,
        gamepad_mode: Option<bool>,
        position_mode: Option<&str>,
        lang: Option<&str>,
        hotkey_label: Option<&str>,
        hold_progress: f32,
        spot_image: Option<&[u8]>,
        guide_hotkey_label: Option<&str>,
    ) -> bool {
        let Some(_) = self.ensure_started() else {
            return false;
        };
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.guidance = Some(guidance);
            if let Some(wr) = window_rect {
                g.window_rect = Some(wr);
            }
            if let Some(wa) = work_area {
                g.work_area = Some(wa);
            }
            if let Some(gp) = gamepad_mode {
                g.gamepad_mode = gp;
            }
            if let Some(pm) = position_mode {
                g.position_mode = norm_position_mode(Some(pm));
            }
            if let Some(l) = lang {
                g.lang = Some(l.to_owned());
            }
            if let Some(hk) = hotkey_label {
                g.hotkey_label = hk.to_owned();
            }
            if let Some(ghk) = guide_hotkey_label {
                g.guide_hotkey_label = ghk.to_owned();
            }
            g.hold_progress = if hold_progress.is_finite() {
                hold_progress.clamp(0.0, 1.0)
            } else {
                0.0
            };
            g.spot_image = spot_image.map(|b| Arc::new(b.to_vec()));
            g.want_visible = true;
            g.state_version += 1;
        }
        self.host.post_message(WM_APP_UPDATE, 0, 0)
    }

    pub fn set_window_rect(&self, window_rect: Option<RectI32>) {
        if let Some(wr) = window_rect {
            self.shared
                .lock()
                .expect("nav hud shared lock poisoned")
                .window_rect = Some(wr);
        }
    }

    pub fn hide(&self) -> bool {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.want_visible = false;
            g.guide_zoomed = false;
        }
        self.host.post_message(WM_APP_HIDE, 0, 0)
    }

    pub fn close(&self) {
        {
            let mut g = self.shared.lock().expect("nav hud shared lock poisoned");
            g.closed = true;
            g.want_visible = false;
            g.edit_mode = false;
        }
        self.host.close();
    }
}

/// Utility helper for timestamping monotonic debug/diagnostics if needed (zero wall-clock leaks).
#[allow(dead_code)]
fn _unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Comprehensive Deterministic Headless & Off-screen Win32 Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layered::{
        EX_STYLE_CLICK_THROUGH, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, premultiplied_rgba_to_bgra,
    };
    use gh_core::nav_controller::{NavPin, NavRouteMode, NavigationController};
    use gh_win32::game_settings::get_window_display_affinity;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Frame, ImageEncoder, codecs::png::PngEncoder};
    use std::ffi::c_void;
    use std::time::Duration;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{
        GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, WS_EX_TRANSPARENT,
    };

    fn encode_synthetic_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&rgba);
        }
        let mut out = Vec::new();
        let enc = PngEncoder::new(&mut out);
        enc.write_image(&buf, w, h, image::ExtendedColorType::Rgba8)
            .expect("encode synthetic PNG");
        out
    }

    fn encode_synthetic_gif(w: u32, h: u32, frames_spec: &[([u8; 4], u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new_with_speed(&mut out, 30);
            enc.set_repeat(Repeat::Infinite).expect("set repeat");
            for &(rgba, delay_ms) in frames_spec {
                let mut buf = vec![0u8; (w * h * 4) as usize];
                for px in buf.as_chunks_mut::<4>().0 {
                    px.copy_from_slice(&rgba);
                }
                let img = RgbaImage::from_raw(w, h, buf).expect("gif frame rgba");
                let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
                enc.encode_frame(frame).expect("encode gif frame");
            }
        }
        out
    }

    fn assert_alpha_and_pm_integrity(pm: &Pixmap, badge_rect: [i32; 4], label: &str) {
        let w = pm.width() as i32;
        let h = pm.height() as i32;
        let data = pm.data();
        assert_eq!(data.len(), (w * h * 4) as usize);

        let [bx, by, bw, bh] = badge_rect;
        let cx = bx + bw / 2;
        let cy = by + bh / 2;

        // 1. Center of compass badge MUST have alpha >= 240
        let center_idx = ((cy * w + cx) * 4) as usize;
        let center_a = data[center_idx + 3];
        assert!(
            center_a >= 240,
            "[{label}] Badge center alpha={center_a} < 240"
        );

        // 2. Inner disc of badge (r <= 0.35 * bw) must have zero sky hole-punching
        let r_sq = (bw as f64 * 0.35).powi(2);
        for py in by..(by + bh) {
            for px in bx..(bx + bw) {
                let d2 = ((px - cx) as f64).powi(2) + ((py - cy) as f64).powi(2);
                if d2 <= r_sq {
                    let a = data[((py * w + px) * 4 + 3) as usize];
                    assert!(a >= 240, "[{label}] Badge inner disc alpha={a} < 240");
                }
            }
        }

        // 3. Card interior strip across text region must stay >= 240 alpha
        let strip_x0 = (bx + bw + 10).min(w - 12);
        let strip_x1 = (w - 14).max(strip_x0 + 4);
        for px in strip_x0..strip_x1 {
            let a = data[((cy * w + px) * 4 + 3) as usize];
            assert!(a >= 240, "[{label}] Card interior alpha={a} < 240 at x={px}");
        }

        // 4. Premultiplied RGBA invariant and BGRA swizzle parity
        for chunk in data.as_chunks::<4>().0 {
            let a = chunk[3];
            assert!(
                chunk[0] <= a && chunk[1] <= a && chunk[2] <= a,
                "[{label}] premultiplied RGBA violated: {:?}",
                chunk
            );
        }
        let bgra = premultiplied_rgba_to_bgra(data).expect("pm RGBA->BGRA");
        for chunk in bgra.as_chunks::<4>().0 {
            let a = chunk[3];
            assert!(chunk[0] <= a && chunk[1] <= a && chunk[2] <= a);
        }

        // 5. Non-uniform RGB check (real content rendered, not blank fill)
        let mean_r =
            data.as_chunks::<4>().0.iter().map(|c| c[0] as f64).sum::<f64>() / (w * h) as f64;
        let var_r = data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| (c[0] as f64 - mean_r).powi(2))
            .sum::<f64>()
            / (w * h) as f64;
        assert!(var_r.sqrt() > 5.0, "[{label}] expected visible contrast");
    }

    #[test]
    fn circular_smoother_wrap_deadband_and_non_finite() {
        let mut sm = CircularAngleSmoother::new(0.35, 1.5);
        let v0 = sm.update(Some(178.0)).expect("initial");
        assert!((v0 - 178.0).abs() < 1e-6);

        for _ in 0..15 {
            let v = sm.update(Some(-178.0)).expect("step");
            assert!(v.abs() > 170.0, "wrapped wrong way: {v}");
        }
        let err = wrap_deg_signed(sm.value().unwrap() - (-178.0));
        assert!(err.abs() < 1.6, "err={err}");

        // 0° / 360° wrap
        sm.reset();
        sm.update(Some(358.0));
        let v_zero = sm.update(Some(2.0)).unwrap();
        assert!(v_zero.abs() < 5.0, "0/360 wrap snapped: {v_zero}");

        // Exact 180° opposition step (alpha=0.5 triggers mag < 1e-6 chord degeneracy)
        let mut sm_opp = CircularAngleSmoother::new(0.5, 1.5);
        sm_opp.update(Some(0.0));
        let opp = sm_opp.update(Some(180.0)).unwrap();
        assert!(opp.abs() > 1.0 && opp.is_finite(), "opp={opp}");

        // Deadband (< 1.5°) hold
        sm.reset();
        sm.update(Some(10.0));
        let held = sm.update(Some(11.1)).unwrap();
        assert_eq!(held, 10.0);
        let moved = sm.update(Some(20.0)).unwrap();
        assert!(moved > 10.0 && moved < 20.0);

        // Non-finite ignored cleanly
        let before = sm.value();
        assert_eq!(sm.update(Some(f64::NAN)), before);
        assert_eq!(sm.update(Some(f64::INFINITY)), before);
    }

    #[test]
    fn noise_deadband_scales_with_distance_and_guards_nan() {
        assert!(noise_deadband_deg(Some(100.0), POSITION_NOISE_M) < 1.5);
        assert!(noise_deadband_deg(Some(8.0), POSITION_NOISE_M) > 8.0);
        assert_eq!(noise_deadband_deg(None, POSITION_NOISE_M), 2.0);
        assert_eq!(noise_deadband_deg(Some(f64::NAN), POSITION_NOISE_M), 2.0);
        assert_eq!(noise_deadband_deg(Some(f64::INFINITY), POSITION_NOISE_M), 2.0);
        assert_eq!(noise_deadband_deg(Some(10.0), f64::NAN), 2.0);
        // -5m clamps to d = 1.0 -> atan2(1.4, 1.0) = 54.46° clamped to 14.0°
        assert_eq!(noise_deadband_deg(Some(-5.0), POSITION_NOISE_M), 14.0);
        assert_eq!(noise_deadband_deg(Some(1000.0), POSITION_NOISE_M), 1.2);
    }

    #[test]
    fn spring_smoother_calm_near_pin_wrap_glide_and_dt_spikes() {
        // Deterministic LCG jitter +/-1.4m at 10m (initial heading near 0°)
        let mut sm = SpringAngleSmoother::default();
        let mut seed: u64 = 0x5555_5555_1111_2222;
        let mut next_u = || -> f64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let frac = ((seed >> 11) as f64) / ((1u64 << 53) as f64);
            (frac * 2.0 - 1.0) * 1.4
        };
        let mut outs = Vec::new();
        let mut raws = Vec::new();
        let mut t = 0.0;
        sm.set_distance(Some(10.0));
        sm.set_target(Some(0.0));
        sm.step_at(t);
        for i in 0..240 {
            let dx = next_u();
            let dy = next_u();
            let raw = dx.atan2(10.0 + dy).to_degrees();
            sm.set_distance(Some(10.0));
            sm.set_target(Some(raw));
            t += 1.0 / 30.0;
            let v = sm.step_at(t).unwrap();
            if i > 30 {
                outs.push(v);
                raws.push(raw);
            }
        }
        let min_v = outs.iter().copied().fold(f64::INFINITY, f64::min);
        let max_v = outs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let spread = max_v - min_v;
        let raw_spread = raws.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - raws.iter().copied().fold(f64::INFINITY, f64::min);
        assert!(
            raw_spread > 14.0 && spread < raw_spread * 0.6,
            "needle jitter spread {spread:.2}° did not suppress raw {raw_spread:.2}°"
        );

        // Within 1.4m circular jitter radius at 10m, needle remains completely calm (< 6.0°)
        let mut sm_calm = SpringAngleSmoother::default();
        sm_calm.set_distance(Some(10.0));
        sm_calm.set_target(Some(0.0));
        sm_calm.step_at(0.0);
        let mut calm_outs = Vec::new();
        for i in 1..120 {
            let theta = (i as f64) * 0.37;
            let dx = 1.2 * theta.sin();
            let dy = 1.2 * theta.cos();
            let raw = dx.atan2(10.0 + dy).to_degrees();
            sm_calm.set_distance(Some(10.0));
            sm_calm.set_target(Some(raw));
            calm_outs.push(sm_calm.step_at(i as f64 / 30.0).unwrap());
        }
        let calm_spread = calm_outs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - calm_outs.iter().copied().fold(f64::INFINITY, f64::min);
        assert!(calm_spread < 6.0, "calm spread {calm_spread:.2}° >= 6.0°");

        // Big retarget glides across +/-180 without zero-crossing
        let mut sm2 = SpringAngleSmoother::default();
        sm2.set_distance(Some(80.0));
        sm2.set_target(Some(170.0));
        sm2.step_at(0.0);
        sm2.set_target(Some(-170.0));
        let v1 = sm2.step_at(1.0 / 30.0).unwrap();
        assert!(v1.abs() > 165.0, "wrapped wrong way: {v1}");
        let mut last = v1;
        for k in 2..60 {
            last = sm2.step_at(k as f64 / 30.0).unwrap();
        }
        let err = wrap_deg_signed(last - (-170.0));
        assert!(err.abs() < 2.0, "failed to converge: err={err}");

        // dt <= 0 and huge dt spike guards
        let before = sm2.value();
        assert_eq!(sm2.step_with_dt(-0.5), before);
        assert_eq!(sm2.step_with_dt(0.0), before);
        assert_eq!(sm2.step_with_dt(f64::NAN), before);
        let after_spike = sm2.step_with_dt(100.0).unwrap();
        assert!(after_spike.is_finite());
    }

    #[test]
    fn spring_smoother_lock_on_hold_deadband_prevents_straight_flight_wobble() {
        // At 120m distance noise_deadband_deg is 1.2°, which allows +/-2.5° wobble during straight glide.
        let mut sm = SpringAngleSmoother::default();
        sm.set_distance(Some(120.0));
        assert!(sm.deadband() < 2.0);
        sm.set_target(Some(0.0));
        sm.step_at(0.0);
        for (i, wobble) in [2.4, -2.5, 2.8, -2.2, 1.9, -2.9].into_iter().enumerate() {
            sm.set_target(Some(wobble));
            let v = sm.step_at((i + 1) as f64 / 30.0).unwrap();
            assert_eq!(sm.goal(), Some(0.0), "straight-ahead wobble {wobble}° must not move goal");
            assert!(v.abs() < 0.05, "arrow must hold steady during straight flight: v={v}");
        }
        // Outside the +/-12° lock-on cone, normal 1.2° deadband stays responsive.
        sm.set_target(Some(40.0));
        assert_eq!(sm.goal(), Some(40.0));
        sm.set_target(Some(42.0));
        assert_eq!(sm.goal(), Some(42.0));
    }

    #[test]
    fn scalar_smoother_tau_snap_and_dt_bounds() {
        let mut sm = ScalarSmoother::new(0.35);
        assert_eq!(sm.update_at(Some(20.0), 0.0), Some(20.0));
        let v1 = sm.update_at(Some(40.0), 0.35).unwrap();
        // 1 - e^-1 ≈ 0.6321 -> 20 + 20 * 0.6321 ≈ 32.64
        assert!((v1 - 32.642).abs() < 0.05, "v1={v1}");
        // Jump > 60m snaps immediately
        assert_eq!(sm.update_at(Some(150.0), 0.40), Some(150.0));
        // Non-finite ignored
        assert_eq!(sm.update_at(Some(f64::NAN), 0.50), Some(150.0));
        assert_eq!(sm.update_at(Some(f64::INFINITY), 0.50), Some(150.0));
        // Negative / huge dt clamped [0.0, 0.5]
        let v_neg = sm.update_at(Some(140.0), 0.10).unwrap();
        assert_eq!(v_neg, 150.0);
        let v_big = sm.update_at(Some(120.0), 500.0).unwrap();
        assert!(v_big > 120.0 && v_big < 150.0);
    }

    #[test]
    fn badge_cache_strictly_under_491520_bytes_and_lru_eviction() {
        let mut r = NavHudRenderer::new();
        for deg in (0..360).step_by(4) {
            let rel = ((deg + 180) % 360 - 180) as f64;
            r.get_compass_badge(HudPalette::Info, Some(rel), 1.0, true, false, false)
                .unwrap();
        }
        let s = r.stats();
        assert_eq!(s.badge_cache_count, 90);
        assert_eq!(s.badge_cache_bytes, 90 * 36 * 36 * 4);
        assert!(s.badge_cache_bytes <= MAX_BADGE_CACHE_BYTES);

        // Sweep 4 palettes x 90 angles = 360 entries: byte budget never exceeded
        for pal in [
            HudPalette::Info,
            HudPalette::Success,
            HudPalette::Warning,
            HudPalette::LockOn,
        ] {
            for deg in (0..360).step_by(4) {
                r.get_compass_badge(
                    pal,
                    Some(deg as f64),
                    1.0,
                    true,
                    pal == HudPalette::LockOn,
                    false,
                )
                .unwrap();
                assert!(r.stats().badge_cache_bytes <= MAX_BADGE_CACHE_BYTES);
            }
        }
        assert!(r.stats().badge_evictions > 0);

        // Small custom budget LRU keep + single-entry > budget guard
        let mut tiny = NavHudRenderer::new().with_badge_cache_max_bytes(2 * 36 * 36 * 4);
        tiny.get_compass_badge(HudPalette::Info, Some(0.0), 1.0, true, false, false)
            .unwrap();
        tiny.get_compass_badge(HudPalette::Info, Some(4.0), 1.0, true, false, false)
            .unwrap();
        // Touch 0° so 4° is LRU
        let (_, hit, _) = tiny
            .get_compass_badge(HudPalette::Info, Some(0.0), 1.0, true, false, false)
            .unwrap();
        assert!(hit);
        tiny.get_compass_badge(HudPalette::Info, Some(8.0), 1.0, true, false, false)
            .unwrap();
        let (_, hit_0, _) = tiny
            .get_compass_badge(HudPalette::Info, Some(0.0), 1.0, true, false, false)
            .unwrap();
        let (_, hit_4, _) = tiny
            .get_compass_badge(HudPalette::Info, Some(4.0), 1.0, true, false, false)
            .unwrap();
        assert!(hit_0);
        assert!(!hit_4);

        // Oversized single entry (> budget) must leave cache at 0 bytes
        let mut micro = NavHudRenderer::new().with_badge_cache_max_bytes(1024);
        micro
            .get_compass_badge(HudPalette::Info, Some(0.0), 1.0, true, false, false)
            .unwrap();
        assert_eq!(micro.stats().badge_cache_bytes, 0);
        assert_eq!(micro.stats().badge_cache_count, 0);
    }

    #[test]
    fn two_layer_prebake_steady_state_hits_zero_reshape() {
        let mut r = NavHudRenderer::new();
        let pin = NavPin::new("t1", "Rương Hiếm Đồng Bằng Phong Khởi", -28.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![pin]);
        let g = ctrl.compute_guidance(0.0, 0.0, Some(45.0), 2, None, false, "TRACKING", "");

        // Warm up 90 angles
        for deg in (0..360).step_by(4) {
            let rel = ((deg + 180) % 360 - 180) as f64;
            let opts = NavRenderOptions {
                delta_deg: Some(rel),
                lang: Some("vi"),
                ..Default::default()
            };
            r.render(&g, &opts).unwrap();
        }
        let after_warm = r.stats();
        assert_eq!(after_warm.card_builds, 1);
        let shape_calls_after_warm = after_warm.text_shape_calls;

        let mut buf = Vec::new();
        for i in 0..500 {
            let rel = (((i * 4) + 180) % 360 - 180) as f64;
            let opts = NavRenderOptions {
                delta_deg: Some(rel),
                lang: Some("vi"),
                ..Default::default()
            };
            r.render_into(&g, &opts, &mut buf).unwrap();
            assert!(r.last_meta().card_cache_hit);
            assert!(r.last_meta().badge_cache_hit);
        }
        let after_steady = r.stats();
        assert_eq!(after_steady.card_builds, 1);
        assert_eq!(after_steady.text_shape_calls, shape_calls_after_warm);
    }

    #[test]
    #[ignore = "Run via `--cargo-profile cv-bench --run-ignored all` for release-speed frame latency check"]
    fn bench_two_layer_steady_state_under_0_4_ms() {
        let mut r = NavHudRenderer::new();
        let pin = NavPin::new("t1", "Rương Hiếm Đồng Bằng Phong Khởi", -28.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![pin]);
        let g = ctrl.compute_guidance(0.0, 0.0, Some(45.0), 2, None, false, "TRACKING", "");

        for deg in (0..360).step_by(4) {
            let rel = ((deg + 180) % 360 - 180) as f64;
            let opts = NavRenderOptions {
                delta_deg: Some(rel),
                lang: Some("vi"),
                ..Default::default()
            };
            r.render(&g, &opts).unwrap();
        }

        let n_frames = 1000;
        let mut buf = Vec::new();
        let t0 = Instant::now();
        for i in 0..n_frames {
            let rel = (((i * 4) + 180) % 360 - 180) as f64;
            let opts = NavRenderOptions {
                delta_deg: Some(rel),
                lang: Some("vi"),
                ..Default::default()
            };
            r.render_into(&g, &opts, &mut buf).unwrap();
            assert!(r.last_meta().card_cache_hit);
            assert!(r.last_meta().badge_cache_hit);
        }
        let avg_ms = t0.elapsed().as_secs_f64() * 1000.0 / n_frames as f64;
        assert!(
            avg_ms < 0.4,
            "Steady-state compass composition avg={avg_ms:.4} ms/frame >= 0.4 ms"
        );
    }

    #[test]
    fn alpha_integrity_and_premultiplied_bgra_across_states() {
        let mut r = NavHudRenderer::new();
        let mut pin = NavPin::new("p1", "Rương Hiếm Đồng Bằng Phong Khởi", -28.0, 0.0);
        pin.image_url = "https://example.invalid/spot.png".to_owned();
        pin.description = "Ẩn sau bụi cây ngay bên cạnh bức tượng cũ trên mỏm đá hướng Bắc.".to_owned();

        let mut ctrl = NavigationController::with_pins(vec![pin.clone()]);
        let g_compact =
            ctrl.compute_guidance(0.0, 0.0, Some(45.0), 2, None, false, "TRACKING", "");
        let g_arrived =
            ctrl.compute_guidance(-20.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        let g_lost = ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "LOST", "");
        let g_mismatch = NavGuidance {
            state: NavState::FloorMismatch,
            target: Some(NavPin {
                id: "fm".into(),
                name: "Rương Hang Động".into(),
                floor_id: Some("B1".into()),
                floor_name: Some("B1".into()),
                is_underground: true,
                ..Default::default()
            }),
            distance_m: 15.0,
            target_distance_m: Some(15.0),
            delta_deg: Some(25.0),
            floor_mismatch: true,
            target_floor_id: Some("B1".into()),
            target_floor_name: "B1".into(),
            target_is_underground: true,
            ..Default::default()
        };

        let png = encode_synthetic_png(160, 120, [68, 130, 196, 255]);
        assert!(r.put_guide_image(&pin.image_url, &png));

        for lang in ["vi", "en"] {
            let c_pm = r
                .render(
                    &g_compact,
                    &NavRenderOptions {
                        delta_deg: Some(40.0),
                        lang: Some(lang),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(!r.last_meta().expanded);
            assert_alpha_and_pm_integrity(&c_pm, r.last_meta().badge_rect, "compact");

            let a_pm = r
                .render(
                    &g_arrived,
                    &NavRenderOptions {
                        delta_deg: Some(10.0),
                        lang: Some(lang),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(r.last_meta().expanded);
            assert!(r.last_meta().has_spot_card);
            assert_alpha_and_pm_integrity(&a_pm, r.last_meta().badge_rect, "arrived_spot");

            let m_pm = r
                .render(
                    &g_mismatch,
                    &NavRenderOptions {
                        lang: Some(lang),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_alpha_and_pm_integrity(&m_pm, r.last_meta().badge_rect, "mismatch");

            let l_pm = r
                .render(
                    &g_lost,
                    &NavRenderOptions {
                        lang: Some(lang),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_alpha_and_pm_integrity(&l_pm, r.last_meta().badge_rect, "lost");

            let e_pm = r
                .render(
                    &g_compact,
                    &NavRenderOptions {
                        lang: Some(lang),
                        edit_mode: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(r.last_meta().palette, HudPalette::Edit);
            assert_alpha_and_pm_integrity(&e_pm, r.last_meta().badge_rect, "edit");

            let guide_pm = r
                .render(
                    &g_compact,
                    &NavRenderOptions {
                        lang: Some(lang),
                        guide_open: true,
                        guide_hotkey_label: "Alt+H",
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(r.last_meta().guide_open);
            assert_alpha_and_pm_integrity(&guide_pm, r.last_meta().badge_rect, "guide");
        }
    }

    #[test]
    fn lock_on_behind_and_radar_visual_cues() {
        let mut r = NavHudRenderer::new();
        let pin = NavPin::new("p1", "Chest", -40.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![pin]);
        let g = ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");

        // Lock-on at delta = 4°
        r.render(
            &g,
            &NavRenderOptions {
                delta_deg: Some(4.0),
                distance_m: Some(40.0),
                lang: Some("en"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r.last_meta().lock_on);
        assert_eq!(r.last_meta().palette, HudPalette::LockOn);

        // Behind at delta = 165° -> >= 12 amber ring pixels
        let behind_pm = r
            .render(
                &g,
                &NavRenderOptions {
                    delta_deg: Some(165.0),
                    distance_m: Some(40.0),
                    lang: Some("en"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(r.last_meta().behind);
        let [bx, by, bw, bh] = r.last_meta().badge_rect;
        let w = behind_pm.width() as i32;
        let data = behind_pm.data();
        let mut amber_count = 0usize;
        for py in by..(by + bh) {
            for px in bx..(bx + bw) {
                let idx = ((py * w + px) * 4) as usize;
                let (rv, gv, bv) = (data[idx], data[idx + 1], data[idx + 2]);
                if rv > 180 && gv > 100 && bv < 80 {
                    amber_count += 1;
                }
            }
        }
        assert!(
            amber_count >= 12,
            "expected >= 12 amber ring pixels, got {amber_count}"
        );

        // Close-range radar at <= 25m
        r.render(
            &g,
            &NavRenderOptions {
                delta_deg: Some(30.0),
                distance_m: Some(18.0),
                lang: Some("vi"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r.last_meta().radar);
        let xy1 = r.last_meta().radar_xy.unwrap();
        r.render(
            &g,
            &NavRenderOptions {
                delta_deg: Some(33.0),
                distance_m: Some(18.3),
                lang: Some("vi"),
                ..Default::default()
            },
        )
        .unwrap();
        let xy2 = r.last_meta().radar_xy.unwrap();
        assert!((xy1[0] - xy2[0]).abs() <= 1 && (xy1[1] - xy2[1]).abs() <= 1);

        // Far (> 25m) disables radar
        r.render(
            &g,
            &NavRenderOptions {
                delta_deg: Some(30.0),
                distance_m: Some(60.0),
                lang: Some("vi"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!r.last_meta().radar);

        // Idle guidance with distance_m = 0.0 and target_distance_m = None MUST NOT trigger radar
        let g_idle = NavGuidance::default();
        r.render(
            &g_idle,
            &NavRenderOptions {
                delta_deg: Some(10.0),
                lang: Some("en"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!r.last_meta().radar);
    }

    #[test]
    fn compact_vs_expanded_heights_and_hold_progress() {
        let mut r = NavHudRenderer::new();
        let pin = NavPin::new("ns", "Rương Hiếm", -28.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![pin.clone()]);
        let g_compact =
            ctrl.compute_guidance(0.0, 0.0, Some(45.0), 2, None, false, "TRACKING", "");
        let compact = r
            .render(
                &g_compact,
                &NavRenderOptions {
                    lang: Some("vi"),
                    ..Default::default()
                },
            )
            .unwrap();

        let g_arr = NavGuidance {
            state: NavState::Arrived,
            target: Some(pin),
            distance_m: 8.0,
            target_distance_m: Some(8.0),
            ..Default::default()
        };
        let arrived = r
            .render(
                &g_arr,
                &NavRenderOptions {
                    lang: Some("vi"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(arrived.height() > compact.height());

        let flash = r
            .render(
                &g_compact,
                &NavRenderOptions {
                    lang: Some("vi"),
                    flash_text: "Đã ghim: Rương Hiếm",
                    flash_kind: HudPalette::Success,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(flash.height() > compact.height());
        assert_eq!(r.last_meta().palette, HudPalette::Success);

        let hold_0 = r
            .render(
                &g_compact,
                &NavRenderOptions {
                    lang: Some("vi"),
                    hold_progress: 0.0,
                    ..Default::default()
                },
            )
            .unwrap();
        let hold_half = r
            .render(
                &g_compact,
                &NavRenderOptions {
                    lang: Some("vi"),
                    hold_progress: 0.5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_ne!(hold_0.data(), hold_half.data());

        // Extreme / NaN hold_progress must not panic
        for bad in [f32::NAN, f32::INFINITY, -1.0, 2.0] {
            r.render(
                &g_compact,
                &NavRenderOptions {
                    hold_progress: bad,
                    ..Default::default()
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn floor_tags_utf8_truncation_and_no_numeric_id_leak() {
        let g_ug_to_sf = NavGuidance {
            state: NavState::FloorMismatch,
            target: Some(NavPin::new("sf", "Rương Mặt Đất", -15.0, 0.0)),
            distance_m: 15.0,
            target_distance_m: Some(15.0),
            floor_mismatch: true,
            player_floor_id: Some("184".into()),
            player_floor_name: "B1".into(),
            player_is_underground: true,
            target_is_underground: false,
            ..Default::default()
        };
        let res_vi = resolve_texts_and_mode(&g_ug_to_sf, "vi", "Alt+K", "Alt+H", None, None);
        assert_eq!(res_vi.floor_tag, "↑ Mặt đất");
        assert_eq!(
            res_vi.subtitle,
            "Khác tầng! (Ngầm (B1) → Mặt đất (G)) • Alt+H hướng dẫn"
        );
        let res_en = resolve_texts_and_mode(&g_ug_to_sf, "en", "Alt+K", "Alt+H", None, None);
        assert_eq!(res_en.floor_tag, "↑ Surface");
        assert_eq!(
            res_en.subtitle,
            "Floor mismatch! (Underground (B1) → Surface (G)) • Alt+H guide"
        );

        let g_raw_id = NavGuidance {
            state: NavState::FloorMismatch,
            target: Some(NavPin {
                id: "ug".into(),
                name: "Rương Ngầm".into(),
                floor_id: Some("184".into()),
                floor_name: None,
                is_underground: true,
                ..Default::default()
            }),
            distance_m: 10.0,
            target_distance_m: Some(10.0),
            floor_mismatch: true,
            player_floor_id: Some("999".into()),
            player_is_underground: false,
            target_floor_id: Some("184".into()),
            target_is_underground: true,
            ..Default::default()
        };
        let r_vi = resolve_texts_and_mode(&g_raw_id, "vi", "Alt+K", "", None, None);
        assert!(!r_vi.subtitle.contains("184") && !r_vi.floor_tag.contains("184"));
        assert_eq!(r_vi.floor_tag, "↓ Ngầm");
        let r_en = resolve_texts_and_mode(&g_raw_id, "en", "Alt+K", "", None, None);
        assert!(!r_en.subtitle.contains("184") && r_en.floor_tag == "↓ UG");

        // B1 -> B2 and B2 -> B1
        let g_b1_b2 = NavGuidance {
            state: NavState::FloorMismatch,
            target: Some(NavPin::new("b2", "P", -5.0, 0.0)),
            floor_mismatch: true,
            player_floor_name: "B1".into(),
            player_is_underground: true,
            target_floor_name: "B2".into(),
            target_is_underground: true,
            ..Default::default()
        };
        assert_eq!(format_floor_tag(&g_b1_b2, "vi"), "↓ B2");

        let g_b2_b1 = NavGuidance {
            player_floor_name: "B2".into(),
            target_floor_name: "B1".into(),
            ..g_b1_b2.clone()
        };
        assert_eq!(format_floor_tag(&g_b2_b1, "vi"), "↑ B1");

        // Surface same-floor tag is empty
        let g_sf = NavGuidance {
            state: NavState::Tracking,
            target: Some(NavPin::new("sf", "Chest", -20.0, 0.0)),
            ..Default::default()
        };
        assert_eq!(format_floor_tag(&g_sf, "vi"), "");

        // Multi-byte Vietnamese floor tag > 12 chars must truncate cleanly on char boundary
        let g_long_vn = NavGuidance {
            state: NavState::FloorMismatch,
            target: Some(NavPin::new("vn", "P", -5.0, 0.0)),
            floor_mismatch: true,
            player_is_underground: false,
            target_floor_name: "Tầng Hầm Độ Sâu Lớn".into(),
            target_is_underground: true,
            ..Default::default()
        };
        let truncated = format_floor_tag(&g_long_vn, "vi");
        assert!(truncated.ends_with('…'));
        assert_eq!(truncated.chars().count(), 12);
    }

    #[test]
    fn bilingual_vi_en_and_label_resolver_override() {
        let mut r = NavHudRenderer::new();
        let resolver: LabelResolver = Arc::new(|_map_id, label_id, lang| match (label_id, lang) {
            (Some(42), "vi") => Some("Khiêu Chiến Hạn Giờ".to_owned()),
            (Some(42), "en") => Some("Time Trial Challenge".to_owned()),
            _ => None,
        });
        r.set_label_resolver(Some(resolver));

        let pin = NavPin {
            id: "tt".into(),
            map_id: 2,
            lat: -25.0,
            lng: 0.0,
            name: "Time Trial".into(),
            category: "Time Trial".into(),
            label_id: Some(42),
            ..Default::default()
        };
        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin),
            distance_m: 25.0,
            target_distance_m: Some(25.0),
            ..Default::default()
        };

        let pm_vi = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(r.last_meta().title.starts_with("Khiêu Chiến Hạn Giờ"));

        let pm_en = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("en"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(r.last_meta().title.starts_with("Time Trial Challenge"));
        assert_ne!(pm_vi.data(), pm_en.data());

        // Specific pin name is NOT overridden
        let specific = NavPin {
            id: "sp".into(),
            map_id: 2,
            name: "Rương Phía Bắc".into(),
            category: "Chest".into(),
            label_id: Some(42),
            ..Default::default()
        };
        let g_sp = NavGuidance {
            state: NavState::Tracking,
            target: Some(specific),
            distance_m: 25.0,
            target_distance_m: Some(25.0),
            ..Default::default()
        };
        r.render(
            &g_sp,
            &NavRenderOptions {
                lang: Some("en"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r.last_meta().title.starts_with("Rương Phía Bắc"));
    }

    #[test]
    fn spot_card_thumbnail_lru_30_and_corrupt_image_guard() {
        let mut r = NavHudRenderer::new();
        let png = encode_synthetic_png(160, 120, [68, 130, 196, 255]);
        for i in 0..35 {
            assert!(r.put_thumbnail(&format!("https://example.invalid/t_{i}.png"), &png));
        }
        assert_eq!(r.stats().thumb_cache_count, 30);

        assert!(!r.put_thumbnail("k", b""));
        assert!(!r.put_thumbnail("k", b"not-an-image-at-all"));

        let pin = NavPin {
            id: "s1".into(),
            name: "Rương".into(),
            image_url: "https://example.invalid/spot.png".into(),
            description: "Ẩn sau bụi cây ngay bên cạnh bức tượng cũ.".into(),
            ..Default::default()
        };
        let g_no_spot = NavGuidance {
            state: NavState::Arrived,
            target: Some(pin.clone()),
            distance_m: 8.0,
            target_distance_m: Some(8.0),
            show_spot_card: false,
            ..Default::default()
        };
        let h_no = r
            .render(
                &g_no_spot,
                &NavRenderOptions {
                    lang: Some("vi"),
                    ..Default::default()
                },
            )
            .unwrap()
            .height();

        assert!(r.put_thumbnail(&pin.image_url, &png));
        let g_spot = NavGuidance {
            show_spot_card: true,
            ..g_no_spot
        };
        let h_spot = r
            .render(
                &g_spot,
                &NavRenderOptions {
                    lang: Some("vi"),
                    ..Default::default()
                },
            )
            .unwrap()
            .height();
        assert!(r.last_meta().has_spot_card);
        assert!(h_spot > h_no + 40, "h_spot={h_spot}, h_no={h_no}");
    }

    #[test]
    fn guide_card_zoom_modes_layout_and_cache_cap() {
        let mut r = NavHudRenderer::new();
        let png = encode_synthetic_png(960, 600, [48, 96, 160, 255]);
        for i in 0..15 {
            assert!(r.put_guide_image(&format!("https://example.invalid/g_{i}.png"), &png));
        }
        assert_eq!(r.stats().guide_cache_count, MAX_GUIDE_CACHE_ENTRIES);

        let pin = NavPin {
            id: "gz1".into(),
            name: "Rương Thường".into(),
            map_id: 2,
            description: "Leo lên mỏm đá phía sau tượng.\nRương nằm sau bụi cây.".into(),
            image_url: "https://example.invalid/g_14.png".into(),
            ..Default::default()
        };
        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin),
            distance_m: 20.0,
            target_distance_m: Some(20.0),
            route_mode: NavRouteMode::Direct,
            ..Default::default()
        };

        let compact = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_zoomed: false,
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        let zoomed = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_zoomed: true,
                    max_guide_w: Some(1344),
                    max_guide_h: Some(648),
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(zoomed.width() > compact.width() && zoomed.height() > compact.height());
    }

    #[test]
    fn non_finite_and_extreme_inputs_never_panic() {
        let mut r = NavHudRenderer::new();
        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(NavPin::new(
                "x",
                "Đoạn_tên_tiếng_Việt_siêu_dài_không_có_dấu_cách_để_kiểm_tra_cắt_dòng_UTF8_an_toàn",
                -10.0,
                0.0,
            )),
            distance_m: f64::NAN,
            target_distance_m: Some(10.0),
            delta_deg: Some(f64::INFINITY),
            ..Default::default()
        };

        assert!(matches!(
            r.render(
                &g,
                &NavRenderOptions {
                    scale: f32::NAN,
                    ..Default::default()
                }
            ),
            Err(HudError::InvalidBuffer(_))
        ));
        assert!(matches!(
            r.render(
                &g,
                &NavRenderOptions {
                    scale: f32::INFINITY,
                    ..Default::default()
                }
            ),
            Err(HudError::InvalidBuffer(_))
        ));

        let pm = r
            .render(
                &g,
                &NavRenderOptions {
                    delta_deg: Some(f64::NEG_INFINITY),
                    distance_m: Some(f64::NAN),
                    hold_progress: f32::NAN,
                    max_guide_w: Some(0),
                    max_guide_h: Some(1_000_000),
                    guide_open: true,
                    guide_zoomed: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(pm.width() > 0 && pm.height() > 0);
    }

    #[test]
    fn position_modes_clamping_and_finish_drag() {
        assert_eq!(
            POSITION_MODES,
            [
                "under_minimap",
                "right_of_minimap",
                "top_center",
                "bottom_center",
                "custom"
            ]
        );
        assert_eq!(
            NavMiniHud::norm_position_mode(Some("top_right")),
            PositionMode::UnderMinimap
        );
        assert_eq!(
            NavMiniHud::norm_custom_pos(Some([1.7, -2.0])),
            Some([0.98, 0.0])
        );
        assert_eq!(NavMiniHud::norm_custom_pos(Some([f64::NAN, 0.5])), None);

        let win = RectI32 {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let wa = (0, 0, 1920, 1040);

        let (ux, uy) = compute_hud_position(
            300,
            60,
            Some(win),
            PositionMode::UnderMinimap,
            false,
            Some(wa),
            None,
        );
        let (rx, ry) = compute_hud_position(
            300,
            60,
            Some(win),
            PositionMode::RightOfMinimap,
            false,
            Some(wa),
            None,
        );
        assert!(rx > ux && ry < uy);

        let (bx, by) = compute_hud_position(
            300,
            60,
            Some(win),
            PositionMode::BottomCenter,
            false,
            Some(wa),
            None,
        );
        assert!((bx + 150 - 960).abs() <= 1 && by > 700);

        let (cx, cy) = compute_hud_position(
            300,
            60,
            Some(win),
            PositionMode::Custom,
            false,
            Some(wa),
            Some([0.5, 0.25]),
        );
        assert_eq!((cx, cy), (960, 270));

        // 1440p scales custom anchor proportionally
        let (cx2, cy2) = compute_hud_position(
            300,
            60,
            Some(RectI32 {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            }),
            PositionMode::Custom,
            false,
            Some((0, 0, 2560, 1400)),
            Some([0.5, 0.25]),
        );
        assert_eq!((cx2, cy2), (1280, 360));

        // Custom without anchor falls back to under_minimap
        assert_eq!(
            compute_hud_position(
                300,
                60,
                Some(win),
                PositionMode::Custom,
                false,
                Some(wa),
                None
            ),
            (ux, uy)
        );

        // 1080p / 1440p / 3440x1440 / Gamepad clear minimap and Boss HP bar (< 0.35 * ww)
        for (ww, wh, gp) in [
            (1920, 1080, false),
            (2560, 1440, false),
            (3440, 1440, false),
            (1920, 1080, true),
        ] {
            let layout = compute_ui_layout(ww as u32, wh as u32, gp).unwrap();
            let mm = layout.minimap_rect;
            let (px, py) = compute_hud_position(
                260,
                64,
                Some(RectI32 {
                    x: 0,
                    y: 0,
                    width: ww,
                    height: wh,
                }),
                PositionMode::UnderMinimap,
                gp,
                Some((0, 0, ww, wh - 40)),
                None,
            );
            assert!(py >= mm.y + mm.height);
            assert!((px + 260) as f64 <= (ww as f64) * 0.35);
        }

        // Tight work_area 8px clamp + negative hud_w/h guard
        let (clx, cly) = compute_hud_position(
            -10,
            -20,
            Some(win),
            PositionMode::UnderMinimap,
            false,
            Some((50, 20, 380, 320)),
            None,
        );
        assert!(clx >= 58 && cly >= 28);

        // finish_drag_to_custom
        let drag_pos = finish_drag_to_custom(
            RectI32 {
                x: 960,
                y: 270,
                width: 300,
                height: 60,
            },
            Some(win),
            (1920, 1080),
        )
        .unwrap();
        assert!((drag_pos[0] - 0.5).abs() < 1e-6 && (drag_pos[1] - 0.25).abs() < 1e-6);
    }

    fn gui_counts() -> (u32, u32) {
        // SAFETY: Process pseudo-handle query for GDI/USER object leak assertions.
        unsafe {
            let proc = GetCurrentProcess();
            (
                GetGuiResources(proc, GR_GDIOBJECTS),
                GetGuiResources(proc, GR_USEROBJECTS),
            )
        }
    }

    fn wait_until<F: Fn() -> bool>(timeout: Duration, pred: F, msg: &str) {
        let deadline = Instant::now() + timeout;
        while !pred() {
            assert!(Instant::now() < deadline, "timed out waiting for: {msg}");
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn wait_render<F: FnMut() -> bool>(hud: &NavMiniHud, pred: F, msg: &str) {
        assert!(
            hud.wait_until_render(Duration::from_secs(15), pred),
            "timed out waiting for: {msg}"
        );
    }

    #[test]
    fn nav_mini_hud_offscreen_wda_dib_reuse_guide_zoom_and_zero_leaks() {
        let hud = NavMiniHud::new(NavHudConfig {
            position_mode: PositionMode::UnderMinimap,
            lang: Some("vi".to_owned()),
            guide_zoom_mode: GuideZoomMode::Cycle,
            screen_offset: (-20000, -20000),
            ..Default::default()
        });

        // Lazy startup invariant: no thread or HWND until update/edit
        assert!(!hud.is_thread_running());
        assert_eq!(hud.hwnd(), None);

        let png = encode_synthetic_png(640, 400, [48, 96, 160, 255]);
        let pin_a = NavPin {
            id: "gz1".into(),
            name: "Rương Thường".into(),
            map_id: 2,
            lat: -25.0,
            lng: 0.0,
            description: "Mở rương dưới gốc cây.".into(),
            image_url: "https://example.invalid/gz1.png".into(),
            ..Default::default()
        };
        let pin_b = NavPin {
            id: "gz2".into(),
            name: "Câu đố nhỏ".into(),
            map_id: 2,
            lat: -35.0,
            lng: 0.0,
            description: "Đẩy hòn đá vào bệ.".into(),
            image_url: "https://example.invalid/gz2.png".into(),
            ..Default::default()
        };
        assert!(hud.put_guide_image(&pin_a.image_url, &png));
        assert!(hud.put_guide_image(&pin_b.image_url, &png));

        let g_a = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin_a),
            distance_m: 35.0,
            target_distance_m: Some(35.0),
            delta_deg: Some(25.0),
            ..Default::default()
        };
        let win = RectI32 {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let wa = (0, 0, 1920, 1040);

        assert!(hud.update(
            g_a.clone(),
            Some(win),
            Some(wa),
            None,
            None,
            None,
            None,
            0.0,
            None,
            None
        ));
        wait_render(
            &hud,
            || hud.is_visible() && hud.surface_realloc_count() >= 1,
            "initial HUD present",
        );

        let hwnd = hud.hwnd().expect("off-screen NavMiniHud must spawn HWND");
        crate::layered::assert_hwnd_offscreen_from_virtual_desktop(hwnd);
        assert!(hud.wda_refresh_count() >= 1);
        let ex_style =
            unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
        assert_eq!(ex_style, EX_STYLE_CLICK_THROUGH);
        assert_eq!(
            get_window_display_affinity(hwnd),
            Some(WDA_EXCLUDEFROMCAPTURE)
        );

        let (gdi_base, user_base) = gui_counts();
        let reallocs_first = hud.surface_realloc_count();
        assert_eq!(reallocs_first, 1);

        // Toggle 0x8015 capture visibility
        hud.set_capture_visible(true);
        wait_until(
            Duration::from_secs(15),
            || get_window_display_affinity(hwnd) == Some(WDA_NONE),
            "WDA_NONE",
        );
        hud.set_capture_visible(false);
        wait_until(
            Duration::from_secs(15),
            || get_window_display_affinity(hwnd) == Some(WDA_EXCLUDEFROMCAPTURE),
            "WDA_EXCLUDEFROMCAPTURE",
        );

        // Rotate angle -> DIBSection reused (realloc_count unchanged)
        let g_rot = NavGuidance {
            delta_deg: Some(120.0),
            ..g_a
        };
        assert!(hud.update(
            g_rot,
            Some(win),
            Some(wa),
            None,
            None,
            None,
            None,
            0.0,
            None,
            None
        ));
        wait_render(
            &hud,
            || hud.last_render_meta().angle_bucket == NavHudRenderer::quantize_angle_bucket(Some(120.0)),
            "rotated badge render",
        );
        assert_eq!(hud.surface_realloc_count(), reallocs_first);

        // Cycle guide zoom state machine + target switch reset + hide() reset
        assert!(hud.toggle_guide() && hud.is_guide_open() && !hud.is_guide_zoomed());
        wait_render(
            &hud,
            || hud.last_render_meta().guide_open && !hud.last_render_meta().guide_zoomed,
            "compact guide card render",
        );

        assert!(hud.toggle_guide() && hud.is_guide_open() && hud.is_guide_zoomed());
        wait_render(
            &hud,
            || hud.last_render_meta().guide_zoomed,
            "zoomed guide card render",
        );

        assert!(!hud.toggle_guide() && !hud.is_guide_open() && !hud.is_guide_zoomed());
        wait_render(
            &hud,
            || !hud.last_render_meta().guide_open,
            "guide closed render",
        );

        hud.set_guide_open(true, Some(true));
        let g_b = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin_b),
            distance_m: 35.0,
            target_distance_m: Some(35.0),
            delta_deg: Some(0.0),
            ..Default::default()
        };
        assert!(hud.update(
            g_b,
            Some(win),
            Some(wa),
            None,
            None,
            None,
            None,
            0.0,
            None,
            None
        ));
        wait_render(
            &hud,
            || hud.is_guide_open() && !hud.is_guide_zoomed() && hud.last_render_meta().title.contains("Câu đố nhỏ"),
            "target switch reset guide_zoomed",
        );

        hud.set_guide_open(false, None);
        assert_eq!(hud.set_guide_zoom_mode("compact"), GuideZoomMode::Compact);
        assert!(hud.toggle_guide() && !hud.is_guide_zoomed());
        assert!(!hud.toggle_guide() && !hud.is_guide_open());

        assert_eq!(hud.set_guide_zoom_mode("large"), GuideZoomMode::Large);
        assert!(hud.toggle_guide() && hud.is_guide_zoomed());

        // Edit mode clears WS_EX_TRANSPARENT and ignores WM_APP_HIDE
        assert!(hud.set_edit_mode(true));
        wait_render(
            &hud,
            || {
                let st = unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
                (st & WS_EX_TRANSPARENT.0) == 0
            },
            "edit mode interactive style",
        );
        crate::layered::assert_hwnd_offscreen_from_virtual_desktop(hwnd);
        let hide_seq_before = hud.hide_ack_seq();
        assert!(hud.hide());
        wait_render(
            &hud,
            || hud.hide_ack_seq() > hide_seq_before,
            "WM_APP_HIDE processed during edit mode",
        );
        assert!(hud.is_visible(), "WM_APP_HIDE must be ignored during edit mode");

        assert!(!hud.set_edit_mode(false));
        wait_render(
            &hud,
            || {
                let st = unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
                (st & WS_EX_TRANSPARENT.0) != 0
            },
            "edit mode click-through restored",
        );

        assert!(hud.hide());
        wait_render(
            &hud,
            || !hud.is_visible() && !hud.is_guide_zoomed(),
            "hud hidden",
        );

        hud.close();
        assert_eq!(hud.hwnd(), None);
        assert!(!hud.is_visible());

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut counts = gui_counts();
        while (counts.0 > gdi_base || counts.1 > user_base) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            counts = gui_counts();
        }
        assert!(
            counts.0 <= gdi_base,
            "GDI handle leak: before={gdi_base}, after={}",
            counts.0
        );
        assert!(
            counts.1 <= user_base,
            "USER handle leak: before={user_base}, after={}",
            counts.1
        );
    }

    #[test]
    fn circular_smoother_exact_180_reversal_converges_within_30_steps() {
        let pairs = [(0.0, 180.0), (180.0, 0.0), (90.0, -90.0), (45.0, -135.0)];
        for (start, target) in pairs {
            // Default alpha = 0.35, deadband = 1.5
            let mut sm = CircularAngleSmoother::default();
            sm.update(Some(start));
            let first = sm.update(Some(target)).expect("first step");
            let moved_from_start = wrap_deg_signed(first - start).abs();
            assert!(
                moved_from_start > 10.0,
                "alpha=0.35 stalled on first step {start}->{target}: first={first}"
            );
            for _ in 1..30 {
                sm.step();
            }
            let final_val = sm.value().expect("final value");
            let err = wrap_deg_signed(final_val - target).abs();
            assert!(
                err < 2.0,
                "alpha=0.35 failed to converge within 30 steps ({start}->{target}): val={final_val}, err={err}"
            );

            // Also verify alpha = 0.25 and alpha = 0.10 escape antipodal stall
            let mut sm_25 = CircularAngleSmoother::new(0.25, 1.5);
            sm_25.update(Some(start));
            for _ in 0..30 {
                sm_25.update(Some(target));
            }
            let err_25 = wrap_deg_signed(sm_25.value().unwrap() - target).abs();
            assert!(
                err_25 < 2.0,
                "alpha=0.25 failed to converge within 30 steps ({start}->{target}): err={err_25}"
            );

            let mut sm_10 = CircularAngleSmoother::new(0.10, 1.5);
            sm_10.update(Some(start));
            for _ in 0..30 {
                sm_10.update(Some(target));
            }
            let err_10_mid = wrap_deg_signed(sm_10.value().unwrap() - target).abs();
            assert!(
                err_10_mid < 35.0,
                "alpha=0.10 stalled at 180 ({start}->{target}): err={err_10_mid}"
            );
            for _ in 0..30 {
                sm_10.step();
            }
            let err_10_end = wrap_deg_signed(sm_10.value().unwrap() - target).abs();
            assert!(
                err_10_end < 2.0,
                "alpha=0.10 failed to converge by 60 steps ({start}->{target}): err={err_10_end}"
            );
        }
    }

    #[test]
    fn steady_state_lru_hits_have_zero_unbounded_growth() {
        // 1. ByteBoundedBadgeLru steady-state hit endurance + LRU order check
        let mut badge_lru = ByteBoundedBadgeLru::new(MAX_BADGE_CACHE_BYTES);
        let keys: Vec<BadgeKey> = (0..90)
            .map(|bucket| BadgeKey {
                palette: HudPalette::Info,
                bucket,
                directional: true,
                lock_on: false,
                behind: false,
                badge_px: 36,
            })
            .collect();
        for k in &keys {
            let pm = Pixmap::new(36, 36).unwrap();
            badge_lru.insert(k.clone(), pm);
        }
        let badge_cap_before = badge_lru.map.capacity();
        for i in 0..60_000 {
            assert!(badge_lru.get(&keys[i % keys.len()]).is_some());
        }
        assert_eq!(badge_lru.len(), 90);
        assert_eq!(badge_lru.map.capacity(), badge_cap_before);
        assert_eq!(badge_lru.bytes(), 90 * 36 * 36 * 4);
        assert_eq!(badge_lru.evictions(), 0);

        // Verify LRU eviction accuracy after heavy hit cycling
        let mut small_badge = ByteBoundedBadgeLru::new(2 * 4 * 4 * 4);
        let k_a = BadgeKey {
            palette: HudPalette::Info,
            bucket: 0,
            directional: true,
            lock_on: false,
            behind: false,
            badge_px: 4,
        };
        let k_b = BadgeKey {
            bucket: 1,
            ..k_a.clone()
        };
        let k_c = BadgeKey {
            bucket: 2,
            ..k_a.clone()
        };
        small_badge.insert(k_a.clone(), Pixmap::new(4, 4).unwrap());
        small_badge.insert(k_b.clone(), Pixmap::new(4, 4).unwrap());
        for _ in 0..1000 {
            assert!(small_badge.get(&k_a).is_some());
        }
        small_badge.insert(k_c.clone(), Pixmap::new(4, 4).unwrap());
        assert!(small_badge.get(&k_a).is_some());
        assert!(small_badge.get(&k_b).is_none());
        assert!(small_badge.get(&k_c).is_some());

        // 2. LruMap steady-state hit endurance + zero-clone borrowed lookup + eviction order
        let mut lru: LruMap<String, usize> = LruMap::new(32);
        let str_keys: Vec<String> = (0..32).map(|i| format!("key_{i}")).collect();
        for (idx, k) in str_keys.iter().enumerate() {
            lru.insert(k.clone(), idx);
        }
        let lru_cap_before = lru.map.capacity();
        for i in 0..60_000 {
            let k = str_keys[i % str_keys.len()].as_str();
            assert_eq!(lru.get(k), Some(&(i % str_keys.len())));
        }
        assert_eq!(lru.len(), 32);
        assert_eq!(lru.map.capacity(), lru_cap_before);
        // Touch key_0 so key_1 becomes LRU victim
        assert_eq!(lru.get("key_0"), Some(&0));
        lru.insert("key_new".to_owned(), 999);
        assert_eq!(lru.len(), 32);
        assert_eq!(lru.get("key_0"), Some(&0));
        assert_eq!(lru.get("key_1"), None);

        // 3. NavHudRenderer::render_into over 54,000 steady-state frames
        let mut r = NavHudRenderer::new();
        let pin = NavPin::new("p_steady", "Rương Hiếm Đồng Bằng Phong Khởi", -28.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![pin]);
        let g = ctrl.compute_guidance(0.0, 0.0, Some(45.0), 2, None, false, "TRACKING", "");
        let mut buf = Vec::new();

        for deg in (0..360).step_by(4) {
            let rel = ((deg + 180) % 360 - 180) as f64;
            let opts = NavRenderOptions {
                delta_deg: Some(rel),
                lang: Some("vi"),
                ..Default::default()
            };
            r.render_into(&g, &opts, &mut buf).unwrap();
        }

        let card_cap = r.card_cache.map.capacity();
        let badge_cap = r.badge_cache.map.capacity();
        let radar_cap = r.radar_cache.map.capacity();
        let shapes_warm = r.stats().text_shape_calls;

        for i in 0..54_000 {
            let rel = (((i * 4) + 180) % 360 - 180) as f64;
            let opts = NavRenderOptions {
                delta_deg: Some(rel),
                lang: Some("vi"),
                ..Default::default()
            };
            r.render_into(&g, &opts, &mut buf).unwrap();
            assert!(r.last_meta().card_cache_hit);
            assert!(r.last_meta().badge_cache_hit);
        }

        let after = r.stats();
        assert_eq!(r.card_cache.map.capacity(), card_cap);
        assert_eq!(r.badge_cache.map.capacity(), badge_cap);
        assert_eq!(r.radar_cache.map.capacity(), radar_cap);
        assert_eq!(after.card_builds, 1);
        assert_eq!(after.badge_builds, 90);
        assert_eq!(after.text_shape_calls, shapes_warm);
        assert_eq!(after.badge_evictions, 0);
    }

    #[test]
    fn animated_gif_decode_layer2_compositing_and_zero_card_rebuild() {
        let mut r = NavHudRenderer::new();
        let gif = encode_synthetic_gif(
            160,
            120,
            &[
                ([220, 40, 40, 255], 100),
                ([40, 220, 40, 255], 100),
                ([40, 40, 220, 255], 100),
            ],
        );
        let pin = NavPin {
            id: "gif1".into(),
            name: "Rương GIF".into(),
            map_id: 2,
            description: "Đi theo đường hầm phía dưới.".into(),
            image_url: "https://example.invalid/anim.gif".into(),
            ..Default::default()
        };
        assert!(r.put_guide_image(&pin.image_url, &gif));
        assert_eq!(r.stats().thumb_cache_count, 1);
        assert_eq!(r.stats().guide_cache_count, 1);

        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin),
            distance_m: 30.0,
            target_distance_m: Some(30.0),
            delta_deg: Some(15.0),
            ..Default::default()
        };

        let pm0 = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_frame_index: 0,
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(r.last_meta().guide_animated);
        assert_eq!(r.last_meta().guide_frame_index, 0);
        assert_alpha_and_pm_integrity(&pm0, r.last_meta().badge_rect, "gif_frame_0");
        let builds_after_0 = r.stats().card_builds;
        let shapes_after_0 = r.stats().text_shape_calls;
        assert_eq!(builds_after_0, 1);

        let pm1 = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_frame_index: 1,
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(r.last_meta().card_cache_hit);
        assert!(r.last_meta().guide_animated);
        assert_eq!(r.last_meta().guide_frame_index, 1);
        assert_eq!(r.stats().card_builds, builds_after_0);
        assert_eq!(r.stats().text_shape_calls, shapes_after_0);

        let pm2 = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_frame_index: 2,
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(r.last_meta().card_cache_hit);
        assert_eq!(r.last_meta().guide_frame_index, 2);
        assert_eq!(r.stats().card_builds, builds_after_0);
        assert_ne!(pm0.data(), pm1.data());
        assert_ne!(pm1.data(), pm2.data());
        assert_ne!(pm0.data(), pm2.data());

        // Zoomed guide also switches frames on Layer 2 without rebuilding Layer 1
        let pmz0 = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_zoomed: true,
                    guide_frame_index: 0,
                    max_guide_w: Some(1000),
                    max_guide_h: Some(600),
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        let pmz1 = r
            .render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_zoomed: true,
                    guide_frame_index: 1,
                    max_guide_w: Some(1000),
                    max_guide_h: Some(600),
                    guide_hotkey_label: "Alt+H",
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(pmz0.width() > pm0.width());
        assert!(r.last_meta().card_cache_hit);
        assert_eq!(r.stats().card_builds, 2);
        assert_ne!(pmz0.data(), pmz1.data());
    }

    #[test]
    fn animated_gif_streaming_preserves_all_frames_exact_delays_and_low_ram() {
        let frames_spec: Vec<([u8; 4], u32)> = (0..100)
            .map(|i| {
                let c = ((i * 2) % 250) as u8;
                ([c, 255 - c, 128, 255], 40)
            })
            .collect();
        let gif = encode_synthetic_gif(32, 24, &frames_spec);
        let bundle = decode_guide_bundle(&gif).expect("decode 100-frame gif");
        let GuideMedia::Animated(anim) = bundle.media else {
            panic!("expected animated guide media");
        };
        let uncompressed_100_bytes = 100 * 32 * 24 * 4;
        assert!(anim.nbytes() <= MAX_GUIDE_ANIM_BYTES);
        assert!(
            anim.nbytes() < uncompressed_100_bytes,
            "streaming nbytes={} should stay below 100 uncompressed frames ({uncompressed_100_bytes})",
            anim.nbytes()
        );

        let mut player = StreamingAnimPlayer::new(1, Arc::clone(&anim));
        let mut total_ms = u64::from(player.current_delay_ms);
        assert_eq!(player.frame_index, 0);
        for expected_idx in 1..100 {
            let (idx, delay) = player.step_next();
            assert_eq!(idx, expected_idx);
            assert_eq!(delay, 40);
            total_ms += u64::from(delay);
        }
        assert_eq!(total_ms, 4000);

        // Seamless loop wrap from frame 99 back to frame 0
        let (wrap_idx, wrap_delay) = player.step_next();
        assert_eq!(wrap_idx, 0);
        assert_eq!(wrap_delay, 40);

        // Browser-parity delay normalization
        assert_eq!(normalize_frame_delay_ms(Delay::from_numer_denom_ms(0, 1)), 100);
        assert_eq!(normalize_frame_delay_ms(Delay::from_numer_denom_ms(10, 1)), 100);
        assert_eq!(normalize_frame_delay_ms(Delay::from_numer_denom_ms(16, 1)), 16);
        assert_eq!(normalize_frame_delay_ms(Delay::from_numer_denom_ms(40, 1)), 40);
        assert_eq!(normalize_frame_delay_ms(Delay::from_numer_denom_ms(9999, 1)), 5000);
    }

    #[test]
    fn guide_error_status_renders_image_failed_instead_of_hanging_loading() {
        let mut r = NavHudRenderer::new();
        let pin = NavPin {
            id: "err1".into(),
            name: "Ghim Lỗi".into(),
            map_id: 2,
            image_url: "https://example.invalid/broken.gif".into(),
            ..Default::default()
        };
        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin),
            distance_m: 35.0,
            target_distance_m: Some(35.0),
            ..Default::default()
        };

        for lang in ["vi", "en"] {
            let pm_err = r
                .render(
                    &g,
                    &NavRenderOptions {
                        lang: Some(lang),
                        guide_open: true,
                        guide_status: "error",
                        ..Default::default()
                    },
                )
                .unwrap();
            let pm_load = r
                .render(
                    &g,
                    &NavRenderOptions {
                        lang: Some(lang),
                        guide_open: true,
                        guide_status: "loading",
                        ..Default::default()
                    },
                )
                .unwrap();
            let pm_empty = r
                .render(
                    &g,
                    &NavRenderOptions {
                        lang: Some(lang),
                        guide_open: true,
                        guide_status: "empty",
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_ne!(pm_err.data(), pm_load.data());
            assert_ne!(pm_err.data(), pm_empty.data());
        }
    }

    #[test]
    fn guide_text_names_the_pin_source_for_third_party_pins() {
        assert_eq!(guide_source_name("107596"), "HoYoLAB");
        assert_eq!(guide_source_name("appsample:15130"), "AppSample");
        assert_eq!(guide_source_name("kongying:560"), "Kongying");
        for lang in ["vi", "en"] {
            for key in ["hud.guide.empty", "hud.guide.loading", "hud.guide.image_failed"] {
                let s = gh_core::i18n::t_lang(key, Some(lang), &[("source", "AppSample")]);
                assert!(
                    s.contains("AppSample") && !s.contains("HoYoLAB") && !s.contains("{source}"),
                    "{lang} {key}: {s}"
                );
            }
        }

        // Wiring: the same empty Guide card must read differently for a HoYoLAB and an AppSample pin.
        let mut r = NavHudRenderer::new();
        let mut render_for = |id: &str| {
            let g = NavGuidance {
                state: NavState::Tracking,
                target: Some(NavPin {
                    id: id.into(),
                    name: "Ghim".into(),
                    map_id: 2,
                    ..Default::default()
                }),
                distance_m: 35.0,
                target_distance_m: Some(35.0),
                ..Default::default()
            };
            r.render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_status: "empty",
                    ..Default::default()
                },
            )
            .unwrap()
            .data()
            .to_vec()
        };
        let hoyolab = render_for("107596");
        let appsample = render_for("appsample:15130");
        assert_ne!(hoyolab, appsample, "empty Guide card text must name the pin's source");
    }

    #[test]
    fn guide_footer_shows_the_suggestion_position_and_next_hotkey() {
        for lang in ["vi", "en"] {
            let s = gh_core::i18n::t_lang(
                "hud.guide.suggestion_hint",
                Some(lang),
                &[("index", "2"), ("total", "5"), ("hotkey", "Alt+N")],
            );
            assert!(s.contains("2/5") && s.contains("Alt+N") && !s.contains('{'), "{lang}: {s}");
        }

        let pin = NavPin { id: "appsample:9".into(), name: "Ghim".into(), map_id: 2, ..Default::default() };
        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin),
            distance_m: 35.0,
            target_distance_m: Some(35.0),
            ..Default::default()
        };
        // The position only shows while its pin is the target.
        let raw = ("appsample:9".to_owned(), 2, 5);
        assert_eq!(guide_suggestion_for_target(Some(&raw), &g), Some((2, 5)));
        let other = ("appsample:8".to_owned(), 2, 5);
        assert_eq!(guide_suggestion_for_target(Some(&other), &g), None);
        assert_eq!(guide_suggestion_for_target(None, &g), None);

        let mut r = NavHudRenderer::new();
        let mut render_with = |suggestion: Option<(usize, usize)>| {
            r.render(
                &g,
                &NavRenderOptions {
                    lang: Some("vi"),
                    guide_open: true,
                    guide_status: "ready",
                    guide_hotkey_label: "Alt+H",
                    guide_suggestion: suggestion,
                    guide_next_hotkey_label: "Alt+N",
                    ..Default::default()
                },
            )
            .unwrap()
            .data()
            .to_vec()
        };
        let plain = render_with(None);
        let single = render_with(Some((1, 1)));
        let second = render_with(Some((2, 5)));
        let third = render_with(Some((3, 5)));
        assert_eq!(plain, single, "a single guide needs no position hint");
        assert_ne!(plain, second, "several suggestions show the position hint");
        assert_ne!(second, third, "the hint follows the shown suggestion");
    }

    #[test]
    fn nav_mini_hud_offscreen_advances_animated_gif_frames_without_dib_realloc() {
        let hud = NavMiniHud::new(NavHudConfig {
            position_mode: PositionMode::UnderMinimap,
            lang: Some("vi".to_owned()),
            guide_zoom_mode: GuideZoomMode::Cycle,
            screen_offset: (-20000, -20000),
            ..Default::default()
        });
        let (gdi_base, user_base) = gui_counts();

        let gif = encode_synthetic_gif(
            120,
            90,
            &[
                ([240, 50, 50, 255], 40),
                ([50, 240, 50, 255], 40),
                ([50, 50, 240, 255], 40),
            ],
        );
        let pin = NavPin {
            id: "gif_live".into(),
            name: "Ghim Ảnh Động".into(),
            map_id: 2,
            lat: -25.0,
            lng: 0.0,
            description: "Xem GIF chạy trên HUD.".into(),
            image_url: "https://example.invalid/live.gif".into(),
            ..Default::default()
        };
        assert!(hud.put_guide_image(&pin.image_url, &gif));

        let g = NavGuidance {
            state: NavState::Tracking,
            target: Some(pin),
            distance_m: 35.0,
            target_distance_m: Some(35.0),
            delta_deg: Some(15.0),
            ..Default::default()
        };
        let win = RectI32 {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let wa = (0, 0, 1920, 1040);

        assert!(hud.set_guide_open(true, None));
        assert!(hud.update(g, Some(win), Some(wa), None, None, None, None, 0.0, None, None));
        wait_render(
            &hud,
            || hud.is_visible() && hud.last_render_meta().guide_animated,
            "animated guide card visible",
        );
        let reallocs_open = hud.surface_realloc_count();
        wait_render(
            &hud,
            || hud.last_render_meta().guide_frame_index > 0,
            "animated guide frame advances via render timer",
        );
        assert_eq!(hud.surface_realloc_count(), reallocs_open);

        hud.close();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut counts = gui_counts();
        while (counts.0 > gdi_base || counts.1 > user_base) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            counts = gui_counts();
        }
        assert!(counts.0 <= gdi_base, "GDI handle leak");
        assert!(counts.1 <= user_base, "USER handle leak");
    }

    #[test]
    fn hover_fade_controller_fades_restores_and_stays_opaque_when_suppressed() {
        let mut fade = HoverFadeController::default();
        assert_eq!(fade.current_alpha(), HOVER_FADE_FULL_ALPHA);
        assert_eq!(fade.faded_alpha(), HOVER_FADE_MIN_ALPHA);
        assert!(fade.is_enabled());

        // Not hovered at full alpha -> no change
        assert_eq!(fade.tick(false, false), None);
        assert_eq!(fade.current_alpha(), HOVER_FADE_FULL_ALPHA);

        // Hover smoothly fades down step-by-step to faded_alpha
        let mut prev = fade.current_alpha();
        let mut steps_down = 0usize;
        while let Some(a) = fade.tick(true, false) {
            assert!(a < prev, "expected monotonic fade down: {a} < {prev}");
            assert!(a >= HOVER_FADE_MIN_ALPHA);
            prev = a;
            steps_down += 1;
            assert!(steps_down <= 20, "fade down should settle promptly");
        }
        assert!(steps_down >= 2, "fade down must be smooth, not instant");
        assert_eq!(fade.current_alpha(), HOVER_FADE_MIN_ALPHA);
        assert_eq!(fade.tick(true, false), None);

        // Leave smoothly restores step-by-step to full_alpha
        let mut prev_up = fade.current_alpha();
        let mut steps_up = 0usize;
        while let Some(a) = fade.tick(false, false) {
            assert!(a > prev_up, "expected monotonic restore: {a} > {prev_up}");
            assert!(a <= fade.full_alpha());
            prev_up = a;
            steps_up += 1;
            assert!(steps_up <= 20, "fade restore should settle promptly");
        }
        assert!(steps_up >= 2, "fade restore must be smooth, not instant");
        assert_eq!(fade.current_alpha(), HOVER_FADE_FULL_ALPHA);

        // Suppressed (edit mode or expanded guide card) stays opaque even when hovered
        assert_eq!(fade.tick(true, true), None);
        assert_eq!(fade.current_alpha(), HOVER_FADE_FULL_ALPHA);

        // If suppressed while partially faded, restores to full opacity immediately
        let mid = fade.tick(true, false).expect("partial fade");
        assert!(mid < HOVER_FADE_FULL_ALPHA);
        assert_eq!(fade.tick(true, true), Some(HOVER_FADE_FULL_ALPHA));
        assert_eq!(fade.current_alpha(), HOVER_FADE_FULL_ALPHA);

        // Disabled auto-fade stays opaque
        fade.set_enabled(false);
        assert_eq!(fade.tick(true, false), None);
        assert_eq!(fade.current_alpha(), HOVER_FADE_FULL_ALPHA);

        // Screen-rect hit-testing boundaries
        let rect = RectI32 {
            x: 100,
            y: 200,
            width: 260,
            height: 64,
        };
        assert!(point_in_rect(100, 200, rect));
        assert!(point_in_rect(359, 263, rect));
        assert!(!point_in_rect(99, 200, rect));
        assert!(!point_in_rect(360, 263, rect));
        assert!(!point_in_rect(100, 264, rect));
        assert!(!point_in_rect(
            100,
            200,
            RectI32 {
                x: 100,
                y: 200,
                width: 0,
                height: 64
            }
        ));
    }
}
