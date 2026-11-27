//! Click-through, non-activating Win32 layered HUD toast overlay (`src/single_monitor.py`).
//!
//! Splits pure rendering (`render_toast_pixmap`), ROI-safe placement (`compute_toast_position`),
//! and fade timeline state (`ToastTimeline`, `ToastStateMachine`) from the lazy Win32 host
//! (`HudToastOverlay` built on `LayeredWindowHost`).

use crate::fonts::{FontWeightRole, HudFonts, TextAlign, TextEffect, TextStyle};
use crate::layered::{
    LayeredHandler, LayeredWindowConfig, LayeredWindowCtx, LayeredWindowHost,
};
use crate::{HudError, HudResult};
use cosmic_text::{Ellipsize, Wrap};
use gh_core::frame::RectI32;
use gh_win32::capture::WindowFinder;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

pub const TOAST_FADE_DURATION_MS: u64 = 350;
pub const TOAST_FADE_TICK_MS: u32 = 25;
pub const TOAST_MIN_DURATION_MS: u64 = 600;
pub const TOAST_MIN_HOLD_MS: u64 = 120;

pub const WM_APP_SHOW: u32 = WM_APP + 1; // 0x8001
pub const TIMER_WAIT: usize = 1;
pub const TIMER_FADE: usize = 2;

const CARD_FILL: [u8; 4] = [26, 30, 40, 240];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastKind {
    Success,
    #[default]
    Info,
    Warning,
    Error,
}

impl ToastKind {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "success" => Self::Success,
            "warning" => Self::Warning,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }

    pub const fn palette(self) -> ToastPalette {
        match self {
            Self::Success => ToastPalette {
                accent: [82, 196, 160, 255],
                border: [211, 188, 142, 185],
                badge_bg: [82, 196, 160, 42],
                title: [242, 236, 224, 255],
                sub: [184, 175, 158, 255],
            },
            Self::Info => ToastPalette {
                accent: [211, 188, 142, 255],
                border: [211, 188, 142, 185],
                badge_bg: [211, 188, 142, 38],
                title: [242, 236, 224, 255],
                sub: [184, 175, 158, 255],
            },
            Self::Warning => ToastPalette {
                accent: [224, 171, 88, 255],
                border: [211, 188, 142, 195],
                badge_bg: [224, 171, 88, 44],
                title: [242, 236, 224, 255],
                sub: [204, 190, 162, 255],
            },
            Self::Error => ToastPalette {
                accent: [238, 130, 125, 255],
                border: [211, 188, 142, 195],
                badge_bg: [238, 130, 125, 48],
                title: [248, 228, 224, 255],
                sub: [196, 182, 176, 255],
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastPalette {
    pub accent: [u8; 4],
    pub border: [u8; 4],
    pub badge_bg: [u8; 4],
    pub title: [u8; 4],
    pub sub: [u8; 4],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastRequest {
    pub seq: u64,
    pub title: String,
    pub subtitle: String,
    pub kind: ToastKind,
    pub duration_ms: u64,
    pub anchor_rect: Option<RectI32>,
}

/// Deterministic, HWND-free timeline math for hold + 350 ms linear fade-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastTimeline {
    pub total_ms: u64,
    pub hold_ms: u64,
    pub fade_ms: u64,
}

impl ToastTimeline {
    pub fn new(duration_ms: u64) -> Self {
        let total_ms = duration_ms.max(TOAST_MIN_DURATION_MS);
        let hold_ms = total_ms
            .saturating_sub(TOAST_FADE_DURATION_MS)
            .max(TOAST_MIN_HOLD_MS);
        Self {
            total_ms,
            hold_ms,
            fade_ms: TOAST_FADE_DURATION_MS,
        }
    }

    /// Returns `Some(alpha 0..=255)` while visible, or `None` once `elapsed_ms >= total_ms`.
    pub fn alpha_at(&self, elapsed_ms: u64) -> Option<u8> {
        if elapsed_ms >= self.total_ms {
            return None;
        }
        if elapsed_ms < self.hold_ms {
            return Some(255);
        }
        let rem = ((self.total_ms - elapsed_ms) as f64 / self.fade_ms as f64).clamp(0.0, 1.0);
        Some((255.0 * rem).round() as u8)
    }
}

/// Latest-wins single-slot toast queue and timeline state machine.
#[derive(Debug, Default)]
pub struct ToastStateMachine {
    seq: u64,
    active: Option<(ToastRequest, ToastTimeline)>,
}

impl ToastStateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces any currently active/pending toast (latest-wins) and returns the new sequence ID.
    pub fn push(
        &mut self,
        title: impl Into<String>,
        subtitle: impl Into<String>,
        kind: ToastKind,
        duration_ms: u64,
        anchor_rect: Option<RectI32>,
    ) -> u64 {
        self.seq += 1;
        let req = ToastRequest {
            seq: self.seq,
            title: title.into(),
            subtitle: subtitle.into(),
            kind,
            duration_ms,
            anchor_rect,
        };
        let timeline = ToastTimeline::new(duration_ms);
        self.active = Some((req, timeline));
        self.seq
    }

    pub const fn seq(&self) -> u64 {
        self.seq
    }

    pub fn active_request(&self) -> Option<&ToastRequest> {
        self.active.as_ref().map(|(req, _)| req)
    }

    pub fn active_timeline(&self) -> Option<ToastTimeline> {
        self.active.as_ref().map(|(_, tl)| *tl)
    }

    pub fn alpha_at(&self, elapsed_ms: u64) -> Option<u8> {
        self.active
            .as_ref()
            .and_then(|(_, tl)| tl.alpha_at(elapsed_ms))
    }

    pub fn clear(&mut self) {
        self.active = None;
    }
}

/// Computes responsive toast UI scale (`[0.9, 1.6]`) matching `HudToastOverlay._handle_show_request`.
pub fn compute_toast_scale(anchor_rect: Option<RectI32>, screen_h: i32) -> f32 {
    let anchor_h = match anchor_rect {
        Some(r) if r.width > 200 && r.height > 200 => r.height,
        _ => screen_h.max(1),
    };
    (anchor_h as f32 / 1080.0).clamp(0.9, 1.6)
}

/// Places the toast top-center inside the anchor window (or primary screen),
/// staying within the top ~10% band clear of all CV detection ROIs.
pub fn compute_toast_position(
    toast_w: i32,
    toast_h: i32,
    anchor_rect: Option<RectI32>,
    screen_dims: (i32, i32),
) -> (i32, i32) {
    let _ = toast_h;
    let (left, top, width, height) = match anchor_rect {
        Some(r) if r.width > 200 && r.height > 200 => (r.x, r.y, r.width, r.height),
        _ => (0, 0, screen_dims.0.max(1), screen_dims.1.max(1)),
    };
    let x = left + (width - toast_w) / 2;
    let y = top + ((height as f64 * 0.032).round() as i32).max(22);
    (x, y)
}

/// Renders an anti-aliased Midnight Slate glass pill toast into a premultiplied RGBA `Pixmap`.
pub fn render_toast_pixmap(
    fonts: &mut HudFonts,
    title: &str,
    subtitle: &str,
    kind: ToastKind,
    scale: f32,
) -> HudResult<Pixmap> {
    if !scale.is_finite() {
        return Err(HudError::InvalidBuffer(format!(
            "toast scale must be finite, got {scale}"
        )));
    }
    let s = scale.clamp(0.85, 1.8);
    let pal = kind.palette();

    let title_px = (15.0 * s).round().max(10.0);
    let sub_px = (12.5 * s).round().max(10.0);
    let title_style = TextStyle::new(title_px, FontWeightRole::Bold, pal.title);
    let sub_style = TextStyle::new(sub_px, FontWeightRole::Regular, pal.sub);

    let pad_x = (14.0 * s).round() as i32;
    let pad_y = (10.0 * s).round() as i32;
    let badge_d = (24.0 * s).round() as i32;
    let gap_badge = (11.0 * s).round() as i32;
    let max_text_w = (390.0 * s).round();
    let min_w = (210.0 * s).round() as i32;

    let raw_title = if title.trim().is_empty() {
        "Genshin Helper"
    } else {
        title
    };
    let clean_title = fonts.truncate_line(raw_title, title_style, max_text_w);
    let clean_sub = if subtitle.trim().is_empty() {
        String::new()
    } else {
        fonts.truncate_line(subtitle, sub_style, max_text_w)
    };

    let tm = fonts.measure_text(&clean_title, title_style, None, Wrap::None, Ellipsize::None);
    let title_w = tm.width.ceil() as i32;
    let title_h = (tm.visual_height.max(title_px * 0.72)).ceil() as i32;

    let (sub_w, sub_h, sub_top_off, line_gap) = if !clean_sub.is_empty() {
        let sm = fonts.measure_text(&clean_sub, sub_style, None, Wrap::None, Ellipsize::None);
        (
            sm.width.ceil() as i32,
            (sm.visual_height.max(sub_px * 0.72)).ceil() as i32,
            sm.visual_top_offset,
            (3.0 * s).round() as i32,
        )
    } else {
        (0, 0, 0.0, 0)
    };

    let text_block_w = title_w.max(sub_w);
    let text_block_h = title_h + line_gap + sub_h;

    let w = min_w.max(pad_x * 2 + badge_d + gap_badge + text_block_w + (4.0 * s).round() as i32);
    let h = (badge_d + pad_y * 2).max(text_block_h + pad_y * 2);

    let mut pixmap = Pixmap::new(w as u32, h as u32)
        .ok_or_else(|| HudError::Render(format!("failed to allocate toast pixmap {w}x{h}")))?;

    let radius = if clean_sub.is_empty() {
        (h as f32) * 0.5
    } else {
        (14.0 * s).round()
    };
    let border_w = (1.5 * s).max(1.0);
    let card_path = build_rounded_rect_path(
        0.75,
        0.75,
        (w as f32) - 1.5,
        (h as f32) - 1.5,
        radius,
    )
    .ok_or_else(|| HudError::Render("failed to build toast rounded rect path".into()))?;

    let mut fill_paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    fill_paint.set_color_rgba8(CARD_FILL[0], CARD_FILL[1], CARD_FILL[2], CARD_FILL[3]);
    pixmap.fill_path(
        &card_path,
        &fill_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let mut border_paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    border_paint.set_color_rgba8(pal.border[0], pal.border[1], pal.border[2], pal.border[3]);
    let border_stroke = Stroke {
        width: border_w,
        ..Default::default()
    };
    pixmap.stroke_path(
        &card_path,
        &border_paint,
        &border_stroke,
        Transform::identity(),
        None,
    );
    if let Some(inner_path) = build_rounded_rect_path(
        3.0,
        3.0,
        (w as f32) - 6.0,
        (h as f32) - 6.0,
        (radius - 2.0).max(2.0),
    ) {
        let mut inner_paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        inner_paint.set_color_rgba8(211, 188, 142, 46);
        pixmap.stroke_path(
            &inner_path,
            &inner_paint,
            &Stroke {
                width: (0.9 * s).max(0.7),
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }

    // Status badge rounded plaque
    let bsz = badge_d as f32;
    let cx = (pad_x as f32) + bsz * 0.5;
    let cy = (h as f32) * 0.5;
    let badge_r = bsz * 0.5;
    if let Some(plaque) =
        build_rounded_rect_path(cx - badge_r, cy - badge_r, bsz, bsz, (6.0 * s).round())
    {
        let mut bg_paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        bg_paint.set_color_rgba8(
            pal.badge_bg[0],
            pal.badge_bg[1],
            pal.badge_bg[2],
            pal.badge_bg[3],
        );
        pixmap.fill_path(
            &plaque,
            &bg_paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );

        let mut ring_paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        ring_paint.set_color_rgba8(pal.accent[0], pal.accent[1], pal.accent[2], pal.accent[3]);
        pixmap.stroke_path(
            &plaque,
            &ring_paint,
            &Stroke {
                width: (1.2 * s).max(1.0),
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }

    // Vector badge glyph
    let mut glyph_paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    glyph_paint.set_color_rgba8(pal.accent[0], pal.accent[1], pal.accent[2], pal.accent[3]);
    let glyph_stroke = Stroke {
        width: (2.3 * s).max(1.2),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };

    match kind {
        ToastKind::Success => {
            let mut pb = PathBuilder::new();
            pb.move_to(cx - bsz * 0.22, cy + bsz * 0.02);
            pb.line_to(cx - bsz * 0.04, cy + bsz * 0.20);
            pb.line_to(cx + bsz * 0.24, cy - bsz * 0.18);
            if let Some(p) = pb.finish() {
                pixmap.stroke_path(&p, &glyph_paint, &glyph_stroke, Transform::identity(), None);
            }
        }
        ToastKind::Warning => {
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - bsz * 0.22);
            pb.line_to(cx, cy + bsz * 0.06);
            if let Some(p) = pb.finish() {
                pixmap.stroke_path(&p, &glyph_paint, &glyph_stroke, Transform::identity(), None);
            }
            let r_dot = (1.6 * s).max(1.0);
            if let Some(dot) = PathBuilder::from_circle(cx, cy + bsz * 0.16, r_dot) {
                pixmap.fill_path(&dot, &glyph_paint, FillRule::Winding, Transform::identity(), None);
            }
        }
        ToastKind::Error => {
            let d = bsz * 0.17;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - d, cy - d);
            pb.line_to(cx + d, cy + d);
            pb.move_to(cx - d, cy + d);
            pb.line_to(cx + d, cy - d);
            if let Some(p) = pb.finish() {
                pixmap.stroke_path(&p, &glyph_paint, &glyph_stroke, Transform::identity(), None);
            }
        }
        ToastKind::Info => {
            let r_dot = (4.0 * s).max(1.5);
            if let Some(dot) = PathBuilder::from_circle(cx, cy, r_dot) {
                pixmap.fill_path(&dot, &glyph_paint, FillRule::Winding, Transform::identity(), None);
            }
        }
    }

    // Text block (vertically centered by tight ink height)
    let text_x = (pad_x + badge_d + gap_badge) as f32;
    let text_top = ((h - text_block_h) / 2) as f32;
    let mut pm_mut = pixmap.as_mut();
    fonts.draw_text(
        &mut pm_mut,
        &clean_title,
        text_x,
        text_top - tm.visual_top_offset,
        None,
        TextAlign::Left,
        Wrap::None,
        Ellipsize::None,
        title_style,
        TextEffect::None,
    );
    if !clean_sub.is_empty() {
        let sub_y = text_top + (title_h + line_gap) as f32;
        fonts.draw_text(
            &mut pm_mut,
            &clean_sub,
            text_x,
            sub_y - sub_top_off,
            None,
            TextAlign::Left,
            Wrap::None,
            Ellipsize::None,
            sub_style,
            TextEffect::None,
        );
    }

    Ok(pixmap)
}

pub(crate) fn build_rounded_rect_path(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = radius.clamp(0.0, (w * 0.5).min(h * 0.5));
    let mut pb = PathBuilder::new();
    if r <= 0.01 {
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        return pb.finish();
    }
    let k = 0.552_284_8 * r;
    let x1 = x + w;
    let y1 = y + h;

    pb.move_to(x + r, y);
    pb.line_to(x1 - r, y);
    pb.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x + r, y1);
    pb.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

struct ToastHandler {
    pending: Arc<Mutex<ToastStateMachine>>,
    fonts: HudFonts,
    active_timeline: Option<ToastTimeline>,
    show_start: Option<Instant>,
}

impl ToastHandler {
    fn handle_show(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        let req_opt = {
            let guard = self.pending.lock().expect("toast state lock poisoned");
            guard.active_request().cloned()
        };
        let Some(req) = req_opt else {
            return;
        };

        ctx.kill_timer(TIMER_WAIT);
        ctx.kill_timer(TIMER_FADE);

        let (sw, sh) = WindowFinder::get_screen_dimensions();
        let screen_dims = (if sw > 0 { sw } else { 1920 }, if sh > 0 { sh } else { 1080 });
        let scale = compute_toast_scale(req.anchor_rect, screen_dims.1);
        let Ok(pm) = render_toast_pixmap(
            &mut self.fonts,
            &req.title,
            &req.subtitle,
            req.kind,
            scale,
        ) else {
            return;
        };

        let tw = pm.width() as i32;
        let th = pm.height() as i32;
        let (x, y) = compute_toast_position(tw, th, req.anchor_rect, screen_dims);
        if ctx
            .present_premultiplied_rgba(x, y, tw, th, pm.data(), 255, false)
            .is_err()
        {
            return;
        }

        let tl = ToastTimeline::new(req.duration_ms);
        self.active_timeline = Some(tl);
        self.show_start = Some(Instant::now());
        ctx.set_timer(TIMER_WAIT, tl.hold_ms.min(u32::MAX as u64) as u32);
    }
}

impl LayeredHandler for ToastHandler {
    fn on_app_message(
        &mut self,
        ctx: &mut LayeredWindowCtx<'_>,
        msg: u32,
        _wparam: usize,
        _lparam: isize,
    ) -> Option<isize> {
        if msg == WM_APP_SHOW {
            self.handle_show(ctx);
            Some(0)
        } else {
            None
        }
    }

    fn on_timer(&mut self, ctx: &mut LayeredWindowCtx<'_>, timer_id: usize) {
        if timer_id == TIMER_WAIT {
            ctx.kill_timer(TIMER_WAIT);
            ctx.set_timer(TIMER_FADE, TOAST_FADE_TICK_MS);
            return;
        }
        if timer_id == TIMER_FADE {
            let (Some(tl), Some(start)) = (self.active_timeline, self.show_start) else {
                ctx.kill_timer(TIMER_FADE);
                ctx.hide();
                return;
            };
            let elapsed_ms = start.elapsed().as_millis() as u64;
            match tl.alpha_at(elapsed_ms) {
                Some(alpha) => {
                    ctx.present_alpha(alpha);
                }
                None => {
                    ctx.kill_timer(TIMER_FADE);
                    ctx.hide();
                    self.active_timeline = None;
                    self.show_start = None;
                }
            }
        }
    }

    fn on_destroy(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        ctx.kill_timer(TIMER_WAIT);
        ctx.kill_timer(TIMER_FADE);
    }
}

/// Live Win32 single-monitor layered HUD toast overlay with lazy UI thread startup.
pub struct HudToastOverlay {
    host: LayeredWindowHost,
    pending: Arc<Mutex<ToastStateMachine>>,
}

impl HudToastOverlay {
    pub fn new() -> Self {
        Self::with_screen_offset((0, 0))
    }

    pub fn with_screen_offset(offset: (i32, i32)) -> Self {
        let host = LayeredWindowHost::new(LayeredWindowConfig {
            class_prefix: "GenshinHelperHudToast",
            window_title: "GenshinHelperHUD",
            thread_name: "GH_HudToast",
            screen_offset: offset,
            enable_wda: false,
            initial_capture_visible: false,
            wda_refresh_ms: 0,
            ..LayeredWindowConfig::default()
        });
        Self {
            host,
            pending: Arc::new(Mutex::new(ToastStateMachine::new())),
        }
    }

    pub fn show_toast(
        &self,
        title: &str,
        subtitle: &str,
        kind: ToastKind,
        duration_ms: u64,
        anchor_rect: Option<RectI32>,
    ) -> bool {
        let pending_clone = Arc::clone(&self.pending);
        let Some(_hwnd) = self.host.ensure_started(move || ToastHandler {
            pending: pending_clone,
            fonts: HudFonts::new(),
            active_timeline: None,
            show_start: None,
        }) else {
            return false;
        };

        let seq = {
            let mut guard = self.pending.lock().expect("toast state lock poisoned");
            guard.push(title, subtitle, kind, duration_ms, anchor_rect)
        };
        self.host.post_message(WM_APP_SHOW, seq as usize, 0)
    }

    pub fn is_thread_spawned(&self) -> bool {
        self.host.is_thread_running()
    }

    pub fn raw_hwnd(&self) -> Option<isize> {
        self.host.raw_hwnd()
    }

    pub fn is_visible(&self) -> bool {
        self.host.is_visible()
    }

    pub fn close(&self) {
        self.host.close();
    }
}

impl Default for HudToastOverlay {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layered::{EX_STYLE_CLICK_THROUGH, premultiplied_rgba_to_bgra};
    use std::ffi::c_void;
    use std::time::Duration;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, GetWindowLongPtrW};

    #[test]
    fn toast_renders_all_4_kinds_within_roi_safe_bounds() {
        let mut fonts = HudFonts::new();
        let anchors = [(0, 0, 1920, 1080), (120, 80, 2560, 1440)];

        for kind in [
            ToastKind::Success,
            ToastKind::Info,
            ToastKind::Warning,
            ToastKind::Error,
        ] {
            let pm = render_toast_pixmap(
                &mut fonts,
                "Đã ghim: Rương Hiếm (9m)",
                "Khu vực: Đồng bằng Phong Khởi • Tầng B1",
                kind,
                1.0,
            )
            .expect("render toast");

            let w = pm.width() as i32;
            let h = pm.height() as i32;
            assert!((200..=560).contains(&w), "unexpected toast width {w}");
            assert!((38..=96).contains(&h), "unexpected toast height {h}");

            // Premultiplied RGBA & BGRA invariants + non-trivial RGB variance + max_alpha >= 220.
            let mut max_alpha = 0u8;
            let mut r_sum = 0f64;
            let mut r_sq_sum = 0f64;
            let px_count = pm.pixels().len() as f64;
            for px in pm.pixels() {
                let a = px.alpha();
                max_alpha = max_alpha.max(a);
                assert!(
                    px.red() <= a && px.green() <= a && px.blue() <= a,
                    "toast premultiplied invariant failed"
                );
                let r = px.red() as f64;
                r_sum += r;
                r_sq_sum += r * r;
            }
            assert!(max_alpha >= 220, "max_alpha {max_alpha} too low");
            let mean = r_sum / px_count;
            let std = (r_sq_sum / px_count - mean * mean).max(0.0).sqrt();
            assert!(std > 5.0, "expected visual contrast, got std={std}");

            let bgra = premultiplied_rgba_to_bgra(pm.data()).expect("to bgra");
            assert_eq!(bgra.len(), (w * h * 4) as usize);
            for chunk in bgra.as_chunks::<4>().0 {
                let a = chunk[3];
                assert!(chunk[0] <= a && chunk[1] <= a && chunk[2] <= a);
            }

            // ROI-safe top-center band checks across anchor geometries
            for (rx, ry, rw, rh) in anchors {
                let rect = RectI32 {
                    x: rx,
                    y: ry,
                    width: rw,
                    height: rh,
                };
                let (px, py) = compute_toast_position(w, h, Some(rect), (1920, 1080));
                let center_err = ((px as f64 + w as f64 * 0.5) - (rx as f64 + rw as f64 * 0.5)).abs();
                assert!(center_err <= 1.5, "center_err {center_err}");
                assert!(py >= ry + 20);
                assert!((py + h) as f64 <= ry as f64 + rh as f64 * 0.115);
                assert!(px as f64 >= rx as f64 + rw as f64 * 0.15);
                assert!((px + w) as f64 <= rx as f64 + rw as f64 * 0.85);
            }
        }
    }

    #[test]
    fn toast_truncates_long_vietnamese_text_safely() {
        let mut fonts = HudFonts::new();
        let long_title = "Đã ghim mục tiêu rất dài: Đường hầm ẩn dưới tượng Thất Thiên Phong Khởi ở Mondstadt";
        let long_sub = "Hướng dẫn chi tiết: Đi xuyên qua cổng đá cổ đại rồi rẽ phải xuống hầm ngầm tầng B2";

        let pm = render_toast_pixmap(&mut fonts, long_title, long_sub, ToastKind::Info, 1.0)
            .expect("truncated vietnamese toast");
        assert!(pm.width() <= 560);
        assert!(pm.height() >= 38);

        // Pill mode (empty subtitle)
        let pill = render_toast_pixmap(&mut fonts, "", "", ToastKind::Success, 1.0)
            .expect("empty title fallback pill");
        assert!(pill.width() >= 210);

        // Non-finite scale must return Err(HudError::InvalidBuffer(_)) cleanly
        for bad_scale in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let err = render_toast_pixmap(&mut fonts, "Title", "Sub", ToastKind::Info, bad_scale)
                .expect_err("non-finite scale must fail");
            assert!(matches!(err, HudError::InvalidBuffer(_)));
        }
    }

    #[test]
    fn toast_timeline_alpha_curve_and_replace_semantics() {
        let min_tl = ToastTimeline::new(100);
        assert_eq!(min_tl.total_ms, 600);
        assert_eq!(min_tl.hold_ms, 250);
        assert_eq!(min_tl.alpha_at(0), Some(255));
        assert_eq!(min_tl.alpha_at(249), Some(255));
        assert_eq!(min_tl.alpha_at(600), None);

        let tl = ToastTimeline::new(1000);
        assert_eq!(tl.total_ms, 1000);
        assert_eq!(tl.hold_ms, 650);
        let mut prev = 255u8;
        for t in (650..1000).step_by(25) {
            let a = tl.alpha_at(t).expect("visible during fade");
            assert!(a <= prev, "alpha must decay monotonically at t={t}");
            prev = a;
        }
        assert_eq!(tl.alpha_at(1000), None);

        let mut sm = ToastStateMachine::new();
        let s1 = sm.push("First", "Sub1", ToastKind::Info, 2500, None);
        let s2 = sm.push("Second", "Sub2", ToastKind::Warning, 800, None);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(sm.active_request().expect("active").title, "Second");
        assert_eq!(sm.active_timeline().expect("tl").total_ms, 800);
    }

    #[test]
    fn toast_overlay_offscreen_live_hwnd_lifecycle() {
        let overlay = HudToastOverlay::with_screen_offset((-20000, -20000));
        assert!(!overlay.is_thread_spawned());
        assert_eq!(overlay.raw_hwnd(), None);

        let ok = overlay.show_toast(
            "Đã bật tự động đồng bộ",
            "Bản đồ Teyvat • Không che CV ROI",
            ToastKind::Success,
            1200,
            Some(RectI32 {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
        );
        assert!(ok, "show_toast should succeed offscreen");
        assert!(overlay.is_thread_spawned());
        let hwnd = overlay.raw_hwnd().expect("live toast hwnd");

        let ex_style =
            unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
        assert_eq!(ex_style, EX_STYLE_CLICK_THROUGH);

        assert!(
            overlay.host.wait_until_state(Duration::from_secs(15), || overlay.is_visible()),
            "timed out waiting for toast overlay to become visible"
        );
        assert!(overlay.is_visible());
        crate::layered::assert_hwnd_offscreen_from_virtual_desktop(hwnd);

        overlay.close();
        assert_eq!(overlay.raw_hwnd(), None);
        assert!(!overlay.is_visible());
        assert!(!overlay.is_thread_spawned());
    }
}
