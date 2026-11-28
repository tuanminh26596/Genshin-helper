//! Click-through M-Map route guidance overlay (`WDA_EXCLUDEFROMCAPTURE` by default, switched to
//! `WDA_NONE` only while the app captures the game window via overlay-blind per-window WGC so
//! screen recorders like OBS/ShadowPlay can record the overlay while our M-Map view tracker never
//! sees what is drawn here).
//!
//! Draws an [`MMapGuideScene`] that the app computes from the **registered M-Map view** (what the
//! map currently shows — never from the player position): the route / active section outlined
//! where it really is on the map, an edge arrow toward it while it is off-screen and, once the user
//! has zoomed into the section, a ring on the suggested teleport waypoint with a short popup. The
//! stages mirror the HoYoLAB web flow (whole route → section → teleport).
//!
//! Without a confident view the app stops sending scenes; the window then hides itself after
//! [`LEASE_MS`] (also covers Alt+F4 and a stalled tracking loop).

use crate::fonts::{FontWeightRole, HudFonts, TextAlign, TextEffect, TextStyle};
use crate::layered::{LayeredHandler, LayeredWindowConfig, LayeredWindowCtx, LayeredWindowHost};
use cosmic_text::{Ellipsize, Wrap};
use gh_core::frame::RectI32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, IntSize, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::WM_APP;
#[cfg(not(windows))]
const WM_APP: u32 = 0x8000;

pub const WM_APP_MMAP_UPDATE: u32 = WM_APP + 31;
pub const WM_APP_MMAP_HIDE: u32 = WM_APP + 32;
pub const TIMER_ID_MMAP_LEASE: usize = 201;

/// The window hides itself when no scene arrived for this long (the tracking loop sends one per
/// captured M-Map frame, up to ~60 Hz).
pub const LEASE_MS: u64 = 450;
const LEASE_CHECK_MS: u32 = 150;

const _: () = assert!(WM_APP_MMAP_UPDATE != crate::layered::CAPTURE_AFFINITY_MSG);
const _: () = assert!(WM_APP_MMAP_HIDE != crate::layered::CAPTURE_AFFINITY_MSG);
const _: () = assert!(TIMER_ID_MMAP_LEASE != crate::layered::TIMER_ID_WDA_REFRESH);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MMapOverlayMode {
    /// Route paths + outline + arrow + teleport popup.
    #[default]
    Full,
    /// Outline + arrow + teleport ring only (no route paths, no popup text body).
    Minimal,
    Off,
}

impl MMapOverlayMode {
    pub fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" | "compact" => Self::Minimal,
            "off" | "disabled" | "none" => Self::Off,
            _ => Self::Full,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Minimal => "minimal",
            Self::Off => "off",
        }
    }
}

/// Where the user is in the web-like flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MMapGuideStage {
    /// The M-Map shows another map than the route's.
    WrongMap,
    /// Bring the whole route into view.
    Route,
    /// Route in view: bring / zoom the active section into view.
    Section,
    /// Inside the active section: pick the suggested teleport waypoint.
    Teleport,
    /// No registered view (or the map drawer is open): text banner only.
    Locating,
}

/// One route section drawn as a path (screen px relative to the game client).
#[derive(Debug, Clone, PartialEq)]
pub struct MMapPath {
    pub points: Vec<[f32; 2]>,
    /// Index pairs into `points`; empty = consecutive points.
    pub edges: Vec<(usize, usize)>,
    pub done: Vec<bool>,
    pub color: [u8; 4],
    pub emphasized: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MMapEdgeArrow {
    pub x: f32,
    pub y: f32,
    /// Screen angle (radians, 0 = right, y down) the arrow points at.
    pub angle_rad: f32,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MMapTeleportMarker {
    pub x: f32,
    pub y: f32,
    pub title: String,
    pub subtitle: String,
    pub warn: Option<String>,
}

/// Everything the overlay draws for one frame. Coordinates are game-client px, already rounded
/// by the producer so that an unchanged view yields an equal scene (no redraw).
#[derive(Debug, Clone, PartialEq)]
pub struct MMapGuideScene {
    pub mode: MMapOverlayMode,
    /// Game client rect in screen coordinates.
    pub game_rect: RectI32,
    pub stage: MMapGuideStage,
    pub accent: [u8; 4],
    pub paths: Vec<MMapPath>,
    /// Region the user is guided to (`Route`) or into (`Section`/`Teleport`): x0, y0, x1, y1.
    pub focus_box: Option<[f32; 4]>,
    pub focus_label: String,
    pub arrow: Option<MMapEdgeArrow>,
    pub teleport: Option<MMapTeleportMarker>,
    /// Top-centre banner (wrong map, "zoom into …"): title, subtitle.
    pub banner: Option<(String, String)>,
}

pub fn parse_hex_rgba(hex: &str, alpha: u8) -> [u8; 4] {
    let h = hex.trim().trim_start_matches('#');
    if h.len() >= 6
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        )
    {
        return [r, g, b, alpha];
    }
    [211, 188, 142, alpha]
}

/// UI scale relative to a 1440p client.
fn ui_scale(game_rect: RectI32) -> f32 {
    (game_rect.height as f32 / 1440.0).clamp(0.65, 1.8)
}

const PILL_BG: [u8; 4] = [26, 30, 40, 236];
const TEXT_MAIN: [u8; 4] = [242, 236, 224, 255];
const TEXT_SUB: [u8; 4] = [184, 175, 158, 255];
const WARN: [u8; 4] = [224, 171, 88, 255];
const SHADOW: TextEffect = TextEffect::Shadow {
    offset_x: 1,
    offset_y: 1,
    color: [0, 0, 0, 200],
};

/// Pixel-space layout of a scene: the window rect (relative to the game client) that encloses
/// everything drawn, and the boxes of the text cards inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneLayout {
    pub bounds: [f32; 4],
    pub focus_label_box: Option<[f32; 4]>,
    pub arrow_label_box: Option<[f32; 4]>,
    pub teleport_card_box: Option<[f32; 4]>,
    pub banner_box: Option<[f32; 4]>,
}

fn union(a: Option<[f32; 4]>, b: [f32; 4]) -> Option<[f32; 4]> {
    Some(match a {
        None => b,
        Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
    })
}

fn clip_to(b: [f32; 4], w: f32, h: f32) -> Option<[f32; 4]> {
    let c = [b[0].max(0.0), b[1].max(0.0), b[2].min(w), b[3].min(h)];
    (c[2] - c[0] >= 1.0 && c[3] - c[1] >= 1.0).then_some(c)
}

/// Text-card geometry: (width, height) of a pill holding `lines` at the given sizes.
fn card_size(fonts: &mut HudFonts, lines: &[(&str, TextStyle)], pad: f32, max_w: f32) -> (f32, f32) {
    let mut w: f32 = 0.0;
    let mut h = pad * 2.0;
    for (i, (txt, st)) in lines.iter().enumerate() {
        if txt.is_empty() {
            continue;
        }
        let m = fonts.measure_text(txt, *st, None, Wrap::None, Ellipsize::None);
        w = w.max(m.width);
        h += st.line_height_px + if i > 0 { 2.0 } else { 0.0 };
    }
    ((w + pad * 2.0).min(max_w), h)
}

fn styles(s: f32) -> (TextStyle, TextStyle, TextStyle) {
    (
        TextStyle::new(17.0 * s, FontWeightRole::Bold, TEXT_MAIN),
        TextStyle::new(14.0 * s, FontWeightRole::Semibold, TEXT_SUB),
        TextStyle::new(13.5 * s, FontWeightRole::Semibold, WARN),
    )
}

/// Computes where every element goes (shared by rendering and tests).
pub fn layout_scene(fonts: &mut HudFonts, sc: &MMapGuideScene) -> Option<SceneLayout> {
    if sc.mode == MMapOverlayMode::Off {
        return None;
    }
    let (gw, gh) = (sc.game_rect.width as f32, sc.game_rect.height as f32);
    let s = ui_scale(sc.game_rect);
    let (st_title, st_sub, st_warn) = styles(s);
    let pad = 10.0 * s;
    let max_card_w = 560.0 * s;
    let mut bounds: Option<[f32; 4]> = None;

    if sc.mode == MMapOverlayMode::Full {
        for p in &sc.paths {
            for pt in &p.points {
                let r = 12.0 * s;
                bounds = union(bounds, [pt[0] - r, pt[1] - r, pt[0] + r, pt[1] + r]);
            }
        }
    }

    let mut focus_label_box = None;
    if let Some(fb) = sc.focus_box {
        let (rx, ry) = ellipse_radii(fb, s);
        let (cx, cy) = ((fb[0] + fb[2]) * 0.5, (fb[1] + fb[3]) * 0.5);
        let glow = 10.0 * s;
        bounds = union(bounds, [cx - rx - glow, cy - ry - glow, cx + rx + glow, cy + ry + glow]);
        if !sc.focus_label.is_empty() {
            let (w, h) = card_size(fonts, &[(&sc.focus_label, st_sub)], pad * 0.8, max_card_w);
            let top = (cy - ry - h - 8.0 * s).max(8.0 * s);
            let left = (cx - w * 0.5).clamp(8.0 * s, (gw - w - 8.0 * s).max(8.0 * s));
            let b = [left, top, left + w, top + h];
            focus_label_box = Some(b);
            bounds = union(bounds, b);
        }
    }

    let mut arrow_label_box = None;
    if let Some(a) = &sc.arrow {
        let r = 40.0 * s;
        bounds = union(bounds, [a.x - r, a.y - r, a.x + r, a.y + r]);
        if !a.label.is_empty() {
            let (w, h) = card_size(fonts, &[(&a.label, st_sub)], pad * 0.8, max_card_w);
            // Label sits on the screen-centre side of the arrow.
            let back = a.angle_rad + std::f32::consts::PI;
            let dist_x = (70.0 * s).max(w * 0.5 + 16.0 * s);
            let (lx, ly) = (a.x + back.cos() * dist_x, a.y + back.sin() * 52.0 * s);
            let left = (lx - w * 0.5).clamp(8.0 * s, (gw - w - 8.0 * s).max(8.0 * s));
            let top = (ly - h * 0.5).clamp(8.0 * s, (gh - h - 8.0 * s).max(8.0 * s));
            let b = [left, top, left + w, top + h];
            arrow_label_box = Some(b);
            bounds = union(bounds, b);
        }
    }

    let mut teleport_card_box = None;
    if let Some(t) = &sc.teleport {
        let ring = 40.0 * s;
        bounds = union(bounds, [t.x - ring, t.y - ring, t.x + ring, t.y + ring]);
        let mut lines = [("", st_title); 3];
        lines[0] = (&t.title, st_title);
        let mut n_lines = 1usize;
        if sc.mode == MMapOverlayMode::Full {
            lines[n_lines] = (&t.subtitle, st_sub);
            n_lines += 1;
            if let Some(w) = &t.warn {
                lines[n_lines] = (w, st_warn);
                n_lines += 1;
            }
        }
        let (w, h) = card_size(fonts, &lines[..n_lines], pad, max_card_w);
        // Card to the right of the ring, flipped left near the right edge.
        let gap = 46.0 * s;
        let mut left = t.x + gap;
        if left + w > gw - 8.0 * s {
            left = t.x - gap - w;
        }
        let left = left.clamp(8.0 * s, (gw - w - 8.0 * s).max(8.0 * s));
        let top = (t.y - h * 0.5).clamp(8.0 * s, (gh - h - 8.0 * s).max(8.0 * s));
        let b = [left, top, left + w, top + h];
        teleport_card_box = Some(b);
        bounds = union(bounds, b);
    }

    let mut banner_box = None;
    if let Some((title, sub)) = &sc.banner {
        let (w, h) = card_size(fonts, &[(title, st_title), (sub, st_sub)], pad, max_card_w);
        let left = (gw - w) * 0.5;
        let top = 0.105 * gh;
        let b = [left, top, left + w, top + h];
        banner_box = Some(b);
        bounds = union(bounds, b);
    }

    let bounds = clip_to(bounds?, gw, gh)?;
    let bounds = [bounds[0].floor(), bounds[1].floor(), bounds[2].ceil(), bounds[3].ceil()];
    Some(SceneLayout {
        bounds,
        focus_label_box,
        arrow_label_box,
        teleport_card_box,
        banner_box,
    })
}

/// Ellipse around a focus box (a little larger than the box so the outline does not cover the
/// route itself), with a minimum size so tiny boxes still get a visible ring.
fn ellipse_radii(fb: [f32; 4], s: f32) -> (f32, f32) {
    let rx = ((fb[2] - fb[0]) * 0.5 * 1.18).max(34.0 * s);
    let ry = ((fb[3] - fb[1]) * 0.5 * 1.18).max(34.0 * s);
    (rx, ry)
}

fn paint(col: [u8; 4]) -> Paint<'static> {
    let mut p = Paint {
        anti_alias: true,
        ..Default::default()
    };
    p.set_color_rgba8(col[0], col[1], col[2], col[3]);
    p
}

fn with_alpha(col: [u8; 4], a: u8) -> [u8; 4] {
    [col[0], col[1], col[2], a]
}

fn stroke(width: f32) -> Stroke {
    Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let rad = r.min(w * 0.5).min(h * 0.5).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + rad, y);
    pb.line_to(x + w - rad, y);
    pb.quad_to(x + w, y, x + w, y + rad);
    pb.line_to(x + w, y + h - rad);
    pb.quad_to(x + w, y + h, x + w - rad, y + h);
    pb.line_to(x + rad, y + h);
    pb.quad_to(x, y + h, x, y + h - rad);
    pb.line_to(x, y + rad);
    pb.quad_to(x, y, x + rad, y);
    pb.close();
    pb.finish()
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    pm: &mut Pixmap,
    fonts: &mut HudFonts,
    b: [f32; 4],
    lines: &[(&str, TextStyle)],
    pad: f32,
    border: [u8; 4],
    s: f32,
    t: Transform,
) {
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    if let Some(path) = rounded_rect_path(b[0], b[1], w, h, 9.0 * s) {
        pm.fill_path(&path, &paint(PILL_BG), FillRule::Winding, t, None);
        pm.stroke_path(&path, &paint(border), &stroke((1.6 * s).max(1.0)), t, None);
        let ins = 2.5 * s;
        if let Some(inner) = rounded_rect_path(
            b[0] + ins,
            b[1] + ins,
            (w - ins * 2.0).max(4.0),
            (h - ins * 2.0).max(4.0),
            (7.0 * s).max(2.0),
        ) {
            pm.stroke_path(
                &inner,
                &paint([211, 188, 142, 42]),
                &stroke((0.9 * s).max(0.7)),
                t,
                None,
            );
        }
    }
    let mut y = b[1] + pad;
    for (i, (txt, st)) in lines.iter().enumerate() {
        if txt.is_empty() {
            continue;
        }
        if i > 0 {
            y += 2.0;
        }
        let line = fonts.truncate_line(txt, *st, (w - pad * 2.0).max(8.0));
        fonts.draw_text(
            &mut pm.as_mut(),
            &line,
            b[0] + pad + t.tx,
            y + t.ty,
            Some((w - pad * 2.0).max(8.0)),
            TextAlign::Center,
            Wrap::None,
            Ellipsize::None,
            *st,
            SHADOW,
        );
        y += st.line_height_px;
    }
}

/// Ensures `*slot` holds a transparent `w × h` pixmap, reusing the existing allocation on exact
/// dimension matches via `Pixmap::fill(Color::TRANSPARENT)` and recycling the backing `Vec<u8>`
/// across dimension changes to avoid multi-MB reallocations at 60 fps.
fn acquire_clean_pixmap(slot: &mut Option<Pixmap>, w: u32, h: u32) -> Option<&mut Pixmap> {
    let size = IntSize::from_wh(w, h)?;
    let data_len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    let same_size = slot.as_ref().is_some_and(|pm| pm.width() == w && pm.height() == h);
    if !same_size {
        let pm = match slot.take() {
            Some(prev) => {
                let mut buf = prev.take();
                if buf.len() >= data_len {
                    buf.truncate(data_len);
                    buf.fill(0);
                } else {
                    buf.clear();
                    buf.resize(data_len, 0);
                }
                Pixmap::from_vec(buf, size)?
            }
            None => Pixmap::new(w, h)?,
        };
        *slot = Some(pm);
    } else if let Some(pm) = slot.as_mut() {
        pm.fill(Color::TRANSPARENT);
    }
    slot.as_mut()
}

/// Renders the scene into a reusable pixmap slot covering `layout.bounds` (premultiplied RGBA).
pub fn render_guide_pixmap_into<'a>(
    fonts: &mut HudFonts,
    sc: &MMapGuideScene,
    layout: &SceneLayout,
    reuse: &'a mut Option<Pixmap>,
) -> Option<&'a Pixmap> {
    let b = layout.bounds;
    let w = (b[2] - b[0]).max(1.0) as u32;
    let h = (b[3] - b[1]).max(1.0) as u32;
    let pm = acquire_clean_pixmap(reuse, w, h)?;
    let t = Transform::from_translate(-b[0], -b[1]);
    let s = ui_scale(sc.game_rect);
    let (st_title, st_sub, st_warn) = styles(s);
    let pad = 10.0 * s;
    let accent = sc.accent;

    // Route paths (full mode): dark casing + colour, done stops hollow.
    if sc.mode == MMapOverlayMode::Full {
        for p in sc.paths.iter().filter(|p| !p.emphasized).chain(sc.paths.iter().filter(|p| p.emphasized)) {
            let mut pb = PathBuilder::new();
            if p.edges.is_empty() {
                for seg in p.points.windows(2) {
                    pb.move_to(seg[0][0], seg[0][1]);
                    pb.line_to(seg[1][0], seg[1][1]);
                }
            } else {
                for &(a, c) in &p.edges {
                    if let (Some(pa), Some(pc)) = (p.points.get(a), p.points.get(c)) {
                        pb.move_to(pa[0], pa[1]);
                        pb.line_to(pc[0], pc[1]);
                    }
                }
            }
            let (w_line, alpha) = if p.emphasized { (3.6 * s, 235) } else { (2.2 * s, 120) };
            if let Some(path) = pb.finish() {
                pm.stroke_path(&path, &paint([2, 6, 23, alpha / 2]), &stroke(w_line + 3.0 * s), t, None);
                pm.stroke_path(&path, &paint(with_alpha(p.color, alpha)), &stroke(w_line), t, None);
            }
            let r = if p.emphasized { 5.0 * s } else { 3.4 * s };
            for (i, pt) in p.points.iter().enumerate() {
                if let Some(c) = PathBuilder::from_circle(pt[0], pt[1], r) {
                    let done = p.done.get(i).copied().unwrap_or(false);
                    pm.fill_path(&c, &paint([2, 6, 23, 200]), FillRule::Winding, t, None);
                    if !done {
                        if let Some(inner) = PathBuilder::from_circle(pt[0], pt[1], r * 0.62) {
                            pm.fill_path(&inner, &paint(with_alpha(p.color, alpha)), FillRule::Winding, t, None);
                        }
                    } else {
                        pm.stroke_path(&c, &paint(with_alpha(p.color, alpha)), &stroke(1.4 * s), t, None);
                    }
                }
            }
        }
    }

    // Focus region: soft glowing ellipse + label.
    if let Some(fb) = sc.focus_box {
        let (rx, ry) = ellipse_radii(fb, s);
        let (cx, cy) = ((fb[0] + fb[2]) * 0.5, (fb[1] + fb[3]) * 0.5);
        if let Some(path) = PathBuilder::from_oval(tiny_skia::Rect::from_xywh(cx - rx, cy - ry, rx * 2.0, ry * 2.0)?) {
            let (glow_a, line_a) = if sc.stage == MMapGuideStage::Route { (46, 170) } else { (64, 225) };
            pm.stroke_path(&path, &paint(with_alpha(accent, glow_a)), &stroke(12.0 * s), t, None);
            pm.stroke_path(&path, &paint([2, 6, 23, 150]), &stroke(5.0 * s), t, None);
            pm.stroke_path(&path, &paint(with_alpha(accent, line_a)), &stroke(3.0 * s), t, None);
        }
        if let Some(lb) = layout.focus_label_box {
            draw_card(pm, fonts, lb, &[(&sc.focus_label, st_sub)], pad * 0.8, with_alpha(accent, 220), s, t);
        }
    }

    // Edge arrow: unified symmetrical Tinh Tú (Star Dart) pointing at the target region.
    if let Some(a) = &sc.arrow {
        crate::star_dart::draw_star_dart(
            pm,
            a.x,
            a.y,
            19.0 * s,
            a.angle_rad + std::f32::consts::FRAC_PI_2,
            crate::star_dart::StarDartStyle::EDGE_GOLD,
            t,
            None,
        );
        if let Some(lb) = layout.arrow_label_box {
            draw_card(pm, fonts, lb, &[(&a.label, st_sub)], pad * 0.8, with_alpha(accent, 220), s, t);
        }
    }

    // Teleport: double ring on the waypoint + popup card.
    if let Some(tp) = &sc.teleport {
        for (r, col, w) in [
            (30.0 * s, with_alpha(accent, 90), 9.0 * s),
            (24.0 * s, [2, 6, 23, 160], 4.5 * s),
            (24.0 * s, [255, 255, 255, 240], 2.4 * s),
            (31.0 * s, with_alpha(accent, 230), 2.4 * s),
        ] {
            if let Some(c) = PathBuilder::from_circle(tp.x, tp.y, r) {
                pm.stroke_path(&c, &paint(col), &stroke(w), t, None);
            }
        }
        if let Some(cb) = layout.teleport_card_box {
            let mut lines = [("", st_title); 3];
            lines[0] = (&tp.title, st_title);
            let mut n_lines = 1usize;
            if sc.mode == MMapOverlayMode::Full {
                lines[n_lines] = (&tp.subtitle, st_sub);
                n_lines += 1;
                if let Some(w) = &tp.warn {
                    lines[n_lines] = (w, st_warn);
                    n_lines += 1;
                }
            }
            draw_card(pm, fonts, cb, &lines[..n_lines], pad, with_alpha(accent, 235), s, t);
        }
    }

    if let (Some((title, sub)), Some(bb)) = (&sc.banner, layout.banner_box) {
        let border = if sc.stage == MMapGuideStage::WrongMap { [245, 158, 11, 240] } else { with_alpha(accent, 230) };
        draw_card(pm, fonts, bb, &[(title, st_title), (sub, st_sub)], pad, border, s, t);
    }
    Some(pm)
}

/// Renders the scene into a fresh pixmap covering `layout.bounds` (premultiplied RGBA).
pub fn render_guide_pixmap(fonts: &mut HudFonts, sc: &MMapGuideScene, layout: &SceneLayout) -> Option<Pixmap> {
    let mut slot = None;
    render_guide_pixmap_into(fonts, sc, layout, &mut slot)?;
    slot
}

// ---------------------------------------------------------------------------
// Layered host
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SharedState {
    desired: Option<MMapGuideScene>,
    last_update: Option<Instant>,
    /// Set by the UI thread when it hid the window because the lease expired.
    lease_hidden: bool,
    closed: bool,
}

fn lease_expired(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) > Duration::from_millis(LEASE_MS))
}

struct GuideHandler {
    shared: Arc<Mutex<SharedState>>,
    fonts: HudFonts,
    drawn: Option<MMapGuideScene>,
    pixmap: Option<Pixmap>,
    lease_timer: bool,
}

impl LayeredHandler for GuideHandler {
    fn on_app_message(&mut self, ctx: &mut LayeredWindowCtx<'_>, msg: u32, _wparam: usize, _lparam: isize) -> Option<isize> {
        if msg == WM_APP_MMAP_HIDE {
            self.hide(ctx);
            return Some(0);
        }
        if msg == WM_APP_MMAP_UPDATE {
            self.render(ctx);
            return Some(0);
        }
        None
    }

    fn on_timer(&mut self, ctx: &mut LayeredWindowCtx<'_>, timer_id: usize) {
        if timer_id != TIMER_ID_MMAP_LEASE {
            return;
        }
        let expired = {
            let mut g = self.shared.lock().expect("mmap overlay lock poisoned");
            let e = lease_expired(g.last_update, Instant::now());
            if e {
                g.lease_hidden = true;
            }
            e
        };
        if expired {
            self.hide(ctx);
        }
    }

    fn on_destroy(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        if self.lease_timer {
            ctx.kill_timer(TIMER_ID_MMAP_LEASE);
            self.lease_timer = false;
        }
        self.pixmap = None;
    }
}

impl GuideHandler {
    fn hide(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        if self.lease_timer {
            ctx.kill_timer(TIMER_ID_MMAP_LEASE);
            self.lease_timer = false;
        }
        if ctx.is_visible() || ctx.surface_size() != (0, 0) {
            ctx.hide_and_release_surface();
        }
        self.drawn = None;
        self.pixmap = None;
    }

    fn render(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        let scene = {
            let g = self.shared.lock().expect("mmap overlay lock poisoned");
            if g.closed || lease_expired(g.last_update, Instant::now()) {
                None
            } else {
                g.desired.clone()
            }
        };
        let Some(scene) = scene else {
            self.hide(ctx);
            return;
        };
        if !self.lease_timer {
            ctx.set_timer(TIMER_ID_MMAP_LEASE, LEASE_CHECK_MS);
            self.lease_timer = true;
        }
        if self.drawn.as_ref() == Some(&scene) && ctx.is_visible() {
            return;
        }
        let Some(layout) = layout_scene(&mut self.fonts, &scene) else {
            self.hide(ctx);
            return;
        };
        let Some(pm) = render_guide_pixmap_into(&mut self.fonts, &scene, &layout, &mut self.pixmap) else {
            return;
        };
        let (ox, oy) = ctx.screen_offset();
        let b = layout.bounds;
        let _ = ctx.present_premultiplied_rgba(
            scene.game_rect.x + b[0] as i32 + ox,
            scene.game_rect.y + b[1] as i32 + oy,
            pm.width() as i32,
            pm.height() as i32,
            pm.data(),
            255,
            false,
        );
        self.drawn = Some(scene);
    }
}

pub struct MMapOverlayController {
    shared: Arc<Mutex<SharedState>>,
    host: LayeredWindowHost,
}

impl Default for MMapOverlayController {
    fn default() -> Self {
        Self::new()
    }
}

impl MMapOverlayController {
    pub fn new() -> Self {
        Self::new_with_offset((0, 0))
    }

    pub fn new_with_offset(screen_offset: (i32, i32)) -> Self {
        Self {
            shared: Arc::new(Mutex::new(SharedState::default())),
            host: LayeredWindowHost::new(LayeredWindowConfig {
                class_prefix: "GHMMapGuide",
                window_title: "GenshinHelperMMapGuide",
                thread_name: "GH_MMapGuide",
                screen_offset,
                enable_wda: true,
                initial_capture_visible: false,
                ..Default::default()
            }),
        }
    }

    /// Shows / refreshes the scene; also renews the display lease.
    pub fn update(&self, scene: MMapGuideScene) {
        if scene.mode == MMapOverlayMode::Off || scene.game_rect.width < 320 || scene.game_rect.height < 240 {
            self.hide();
            return;
        }
        let post = {
            let mut g = self.shared.lock().expect("mmap overlay lock poisoned");
            if g.closed {
                return;
            }
            g.last_update = Some(Instant::now());
            let changed = g.desired.as_ref() != Some(&scene);
            let resurrect = std::mem::take(&mut g.lease_hidden);
            if changed {
                g.desired = Some(scene);
            }
            changed || resurrect
        };
        if post {
            let sh = Arc::clone(&self.shared);
            let _ = self.host.ensure_started(move || GuideHandler {
                shared: sh,
                fonts: HudFonts::new(),
                drawn: None,
                pixmap: None,
                lease_timer: false,
            });
            let _ = self.host.post_message(WM_APP_MMAP_UPDATE, 0, 0);
        }
    }

    pub fn hide(&self) {
        {
            let mut g = self.shared.lock().expect("mmap overlay lock poisoned");
            g.desired = None;
            g.last_update = None;
        }
        if self.host.is_thread_running() {
            let _ = self.host.post_message(WM_APP_MMAP_HIDE, 0, 0);
        }
    }

    pub fn close(&self) {
        {
            let mut g = self.shared.lock().expect("mmap overlay lock poisoned");
            g.closed = true;
            g.desired = None;
            g.last_update = None;
        }
        self.host.close();
    }

    pub fn hwnds(&self) -> Vec<isize> {
        self.host.raw_hwnd().into_iter().collect()
    }

    pub fn capture_visible(&self) -> bool {
        self.host.capture_visible()
    }

    /// Updates `WDA_NONE` / `WDA_EXCLUDEFROMCAPTURE` for screen recording visibility.
    /// No-ops cleanly when the affinity already matches `visible` so 60 Hz M-Map ticks stay cheap.
    pub fn set_capture_visible(&self, visible: bool) -> bool {
        if self.host.capture_visible() == visible {
            return visible;
        }
        self.host.set_capture_visible(visible);
        self.host.capture_visible()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(stage: MMapGuideStage) -> MMapGuideScene {
        MMapGuideScene {
            mode: MMapOverlayMode::Full,
            game_rect: RectI32 { x: 0, y: 0, width: 2560, height: 1440 },
            stage,
            accent: [253, 224, 71, 232],
            paths: vec![MMapPath {
                points: vec![[1200.0, 700.0], [1300.0, 760.0], [1380.0, 690.0]],
                edges: vec![],
                done: vec![true, false, false],
                color: [253, 224, 71, 232],
                emphasized: true,
            }],
            focus_box: Some([1180.0, 670.0, 1400.0, 780.0]),
            focus_label: "S2/5 · Vực Hải Sao".into(),
            arrow: None,
            teleport: None,
            banner: None,
        }
    }

    fn alpha_at(pm: &Pixmap, layout: &SceneLayout, x: f32, y: f32) -> u8 {
        let (px, py) = ((x - layout.bounds[0]) as u32, (y - layout.bounds[1]) as u32);
        pm.pixel(px, py).map_or(0, |p| p.alpha())
    }

    #[test]
    fn section_scene_draws_outline_and_path_inside_tight_bounds() {
        let mut fonts = HudFonts::new();
        let sc = scene(MMapGuideStage::Section);
        let layout = layout_scene(&mut fonts, &sc).expect("layout");
        let b = layout.bounds;
        assert!(b[0] > 900.0 && b[2] < 1700.0 && b[1] > 400.0 && b[3] < 1000.0, "bounds hug the content: {b:?}");
        let pm = render_guide_pixmap(&mut fonts, &sc, &layout).expect("pixmap");
        assert!(alpha_at(&pm, &layout, 1300.0, 760.0) > 100, "stop dot drawn");
        let label = layout.focus_label_box.expect("label above the ellipse");
        assert!(label[3] < 670.0);
    }

    #[test]
    fn arrow_label_sits_on_the_screen_centre_side_and_stays_on_screen() {
        let mut fonts = HudFonts::new();
        let mut sc = scene(MMapGuideStage::Route);
        sc.paths.clear();
        sc.focus_box = None;
        sc.arrow = Some(MMapEdgeArrow { x: 2480.0, y: 720.0, angle_rad: 0.0, label: "Tuyến: Băng Tuyết".into() });
        let layout = layout_scene(&mut fonts, &sc).expect("layout");
        let lb = layout.arrow_label_box.expect("label");
        assert!(lb[2] < 2480.0, "label left of a right-pointing edge arrow: {lb:?}");
        let pm = render_guide_pixmap(&mut fonts, &sc, &layout).expect("pixmap");
        assert!(alpha_at(&pm, &layout, 2480.0, 720.0) > 100, "arrow body drawn");
    }

    #[test]
    fn teleport_card_flips_left_near_the_right_edge_and_minimal_mode_skips_paths() {
        let mut fonts = HudFonts::new();
        let mut sc = scene(MMapGuideStage::Teleport);
        sc.teleport = Some(MMapTeleportMarker {
            x: 2450.0,
            y: 300.0,
            title: "Dịch chuyển tại đây".into(),
            subtitle: "S2/5 · Vực Hải Sao".into(),
            warn: Some("Tầng ngầm (B1)".into()),
        });
        let layout = layout_scene(&mut fonts, &sc).expect("layout");
        let cb = layout.teleport_card_box.expect("card");
        assert!(cb[2] < 2450.0, "card flipped left: {cb:?}");
        sc.mode = MMapOverlayMode::Minimal;
        let lm = layout_scene(&mut fonts, &sc).expect("layout");
        let pm = render_guide_pixmap(&mut fonts, &sc, &lm).expect("pixmap");
        assert_eq!(alpha_at(&pm, &lm, 1300.0, 760.0), 0, "no route path in minimal mode");
        assert!(lm.teleport_card_box.expect("card")[3] - lm.teleport_card_box.expect("card")[1]
            < cb[3] - cb[1], "minimal card has only the title");
    }

    #[test]
    fn off_mode_and_empty_scenes_draw_nothing() {
        let mut fonts = HudFonts::new();
        let mut sc = scene(MMapGuideStage::Section);
        sc.mode = MMapOverlayMode::Off;
        assert!(layout_scene(&mut fonts, &sc).is_none());
        let mut empty = scene(MMapGuideStage::Section);
        empty.paths.clear();
        empty.focus_box = None;
        assert!(layout_scene(&mut fonts, &empty).is_none());
    }

    #[test]
    fn locating_banner_is_centred_below_the_top_bar() {
        let mut fonts = HudFonts::new();
        let mut sc = scene(MMapGuideStage::Locating);
        sc.paths.clear();
        sc.focus_box = None;
        sc.banner = Some((
            "Tuyến Băng Tuyết • S2/5 · Vực Hải Sao".into(),
            "Tới Natlan (Teyvat) — kéo bản đồ đến đó để hiện chỉ dẫn".into(),
        ));
        let layout = layout_scene(&mut fonts, &sc).expect("layout");
        let bb = layout.banner_box.expect("banner");
        assert!((bb[0] + bb[2] - 2560.0).abs() < 2.0, "centred: {bb:?}");
        assert!(bb[1] > 0.1 * 1440.0 && bb[3] < 0.25 * 1440.0, "{bb:?}");
        assert!(bb[2] < 0.68 * 2560.0, "clear of the map drawer: {bb:?}");
        let pm = render_guide_pixmap(&mut fonts, &sc, &layout).expect("pixmap");
        assert!(alpha_at(&pm, &layout, 1280.0, (bb[1] + bb[3]) * 0.5) > 150, "card body drawn");
    }

    #[test]
    fn lease_expires_after_the_configured_silence() {
        let t0 = Instant::now();
        assert!(lease_expired(None, t0));
        assert!(!lease_expired(Some(t0), t0 + Duration::from_millis(LEASE_MS - 50)));
        assert!(lease_expired(Some(t0), t0 + Duration::from_millis(LEASE_MS + 1)));
    }

    #[test]
    fn mode_parsing_is_lossy_and_round_trips() {
        assert_eq!(MMapOverlayMode::from_str_lossy("Compact"), MMapOverlayMode::Minimal);
        assert_eq!(MMapOverlayMode::from_str_lossy("none"), MMapOverlayMode::Off);
        assert_eq!(MMapOverlayMode::from_str_lossy("whatever"), MMapOverlayMode::Full);
        for m in [MMapOverlayMode::Full, MMapOverlayMode::Minimal, MMapOverlayMode::Off] {
            assert_eq!(MMapOverlayMode::from_str_lossy(m.as_str()), m);
        }
        assert_eq!(parse_hex_rgba("#38bdf8", 200), [0x38, 0xbd, 0xf8, 200]);
    }

    #[test]
    fn reused_pixmap_preserves_allocation_and_matches_fresh_renders() {
        let mut fonts = HudFonts::new();
        let mut slot: Option<Pixmap> = None;

        // Frame 1: initial render of a section scene.
        let sc1 = scene(MMapGuideStage::Section);
        let l1 = layout_scene(&mut fonts, &sc1).expect("layout 1");
        let fresh1 = render_guide_pixmap(&mut fonts, &sc1, &l1).expect("fresh 1");
        let (ptr1, bytes1) = {
            let pm = render_guide_pixmap_into(&mut fonts, &sc1, &l1, &mut slot).expect("reuse 1");
            (pm.data().as_ptr(), pm.data().to_vec())
        };
        assert_eq!(bytes1, fresh1.data(), "first reused render must match fresh");

        // Frame 2: panned by (+40, +25) px → identical bounds width/height, must reuse in place
        // via Color::TRANSPARENT fill without reallocating or leaving ghost pixels.
        let mut sc2 = scene(MMapGuideStage::Section);
        for p in &mut sc2.paths {
            for pt in &mut p.points {
                pt[0] += 40.0;
                pt[1] += 25.0;
            }
        }
        sc2.focus_box = sc2.focus_box.map(|fb| [fb[0] + 40.0, fb[1] + 25.0, fb[2] + 40.0, fb[3] + 25.0]);
        let l2 = layout_scene(&mut fonts, &sc2).expect("layout 2");
        let dims = |b: [f32; 4]| ((b[2] - b[0]).max(1.0) as u32, (b[3] - b[1]).max(1.0) as u32);
        assert_eq!(
            dims(l2.bounds),
            dims(l1.bounds),
            "translated scene keeps identical pixmap dimensions"
        );
        let fresh2 = render_guide_pixmap(&mut fonts, &sc2, &l2).expect("fresh 2");
        let (ptr2, bytes2) = {
            let pm = render_guide_pixmap_into(&mut fonts, &sc2, &l2, &mut slot).expect("reuse 2");
            (pm.data().as_ptr(), pm.data().to_vec())
        };
        assert_eq!(ptr2, ptr1, "same-size render must reuse pixmap allocation in place");
        assert_eq!(bytes2, fresh2.data(), "reused same-size render must have zero ghost pixels");

        // Frame 3: smaller banner-only scene (recycles backing Vec without growing).
        let mut sc3 = scene(MMapGuideStage::Locating);
        sc3.paths.clear();
        sc3.focus_box = None;
        sc3.banner = Some(("Route".into(), "Drag map to Natlan".into()));
        let l3 = layout_scene(&mut fonts, &sc3).expect("layout 3");
        let fresh3 = render_guide_pixmap(&mut fonts, &sc3, &l3).expect("fresh 3");
        let bytes3 = render_guide_pixmap_into(&mut fonts, &sc3, &l3, &mut slot)
            .expect("reuse 3")
            .data()
            .to_vec();
        assert_eq!(bytes3, fresh3.data(), "resized smaller render must match fresh");

        // Frame 4: larger teleport scene (grows/recycles cleanly).
        let mut sc4 = scene(MMapGuideStage::Teleport);
        sc4.teleport = Some(MMapTeleportMarker {
            x: 1500.0,
            y: 720.0,
            title: "Teleport here".into(),
            subtitle: "S2/5".into(),
            warn: Some("Underground".into()),
        });
        let l4 = layout_scene(&mut fonts, &sc4).expect("layout 4");
        let fresh4 = render_guide_pixmap(&mut fonts, &sc4, &l4).expect("fresh 4");
        let bytes4 = render_guide_pixmap_into(&mut fonts, &sc4, &l4, &mut slot)
            .expect("reuse 4")
            .data()
            .to_vec();
        assert_eq!(bytes4, fresh4.data(), "resized larger render must match fresh");
    }

    #[cfg(windows)]
    #[test]
    fn mmap_overlay_capture_visible_toggles_wda_affinity_cleanly() {
        use crate::layered::{WDA_EXCLUDEFROMCAPTURE, WDA_NONE};
        use gh_win32::game_settings::get_window_display_affinity;

        let ctrl = MMapOverlayController::new_with_offset((-20_000, -20_000));
        assert!(!ctrl.capture_visible(), "default affinity excludes from capture");

        // Pre-start affinity toggle persists into initial window creation.
        assert!(ctrl.set_capture_visible(true));
        assert!(ctrl.capture_visible());
        assert!(!ctrl.set_capture_visible(false));
        assert!(!ctrl.capture_visible());

        ctrl.update(scene(MMapGuideStage::Section));
        let hwnd = (0..1000)
            .find_map(|_| {
                let h = ctrl.hwnds().into_iter().next();
                if h.is_none() {
                    let _ = ctrl
                        .host
                        .wait_until_state(Duration::from_millis(15), || !ctrl.hwnds().is_empty());
                }
                h
            })
            .expect("offscreen mmap overlay hwnd");

        assert_eq!(
            get_window_display_affinity(hwnd),
            Some(WDA_EXCLUDEFROMCAPTURE)
        );

        assert!(ctrl.set_capture_visible(true));
        for _ in 0..1000 {
            if get_window_display_affinity(hwnd) == Some(WDA_NONE) {
                break;
            }
            let _ = ctrl.host.wait_until_state(Duration::from_millis(15), || {
                get_window_display_affinity(hwnd) == Some(WDA_NONE)
            });
        }
        assert_eq!(get_window_display_affinity(hwnd), Some(WDA_NONE));

        assert!(!ctrl.set_capture_visible(false));
        for _ in 0..1000 {
            if get_window_display_affinity(hwnd) == Some(WDA_EXCLUDEFROMCAPTURE) {
                break;
            }
            let _ = ctrl.host.wait_until_state(Duration::from_millis(15), || {
                get_window_display_affinity(hwnd) == Some(WDA_EXCLUDEFROMCAPTURE)
            });
        }
        assert_eq!(
            get_window_display_affinity(hwnd),
            Some(WDA_EXCLUDEFROMCAPTURE)
        );

        ctrl.close();
    }
}
