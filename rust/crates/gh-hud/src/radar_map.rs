//! Native Floating Live Radar Map (60 FPS) — pure-Rust `tiny-skia` + `WS_EX_LAYERED`
//! click-through overlay styled to harmonize with Genshin Impact's celestial UI.
//!
//! Key architectural invariants:
//! - **Zero-overhead when disabled:** `gh-app` holds `Option<RadarMapController>` (`None` when
//!   `radar_enabled == false`) and destroys the window, render thread, decoder thread, and all
//!   RGBA LRU caches on disable.
//! - **North-Up HoYoLAB projection:** Matches HoYoLAB web map & Genshin's locked-North minimap,
//!   with 60 Hz rotating player arrow + camera FOV cone, underground floor overlays (`reflect_y`
//!   honored), waypoints, route legs, and pre-baked HoYoLAB category icons.
//! - **Zero-blank tiles:** Walks parent zoom tiles (`tz - 1 .. min_zoom`) while HD tiles stream in
//!   from the unified `.webp` cache; disk I/O and decoding happen off the 60 Hz render thread.
//! - **CV safety:** Automatically forces `WDA_EXCLUDEFROMCAPTURE` if the radar rectangle overlaps
//!   the top-left minimap/Paimon CV zones, even when OBS/Discord `capture_visible` is enabled.

use crate::fonts::{FontWeightRole, HudFonts, TextAlign, TextEffect, TextStyle};
use crate::layered::{
    LayeredHandler, LayeredWindowConfig, LayeredWindowCtx, LayeredWindowHost, query_monitor_bounds,
};
use crate::nav_hud::{HoverFadeController, wrap_deg_signed};
use cosmic_text::{Ellipsize, Wrap};
use gh_core::config::{
    DEFAULT_RADAR_ZOOM, RADAR_OPACITY_MAX, RADAR_OPACITY_MIN, RADAR_ZOOM_MAX, RADAR_ZOOM_MIN,
};
use gh_core::frame::RectI32;
use gh_core::multimap::{FloorOverlayRecord, MultimapDb};
use gh_core::ui_layout::compute_ui_layout;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_skia::{
    Color, FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap,
    PixmapPaint, Stroke, Transform,
};
#[cfg(windows)]
use windows::Win32::Foundation::POINT;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, WM_APP};
#[cfg(not(windows))]
const WM_APP: u32 = 0x8000;

pub const WM_APP_RADAR_UPDATE: u32 = WM_APP + 41;
pub const WM_APP_RADAR_HIDE: u32 = WM_APP + 42;
pub const WM_APP_RADAR_EDIT: u32 = WM_APP + 43;
pub const WM_APP_RADAR_ASSET_READY: u32 = WM_APP + 44;

pub const TIMER_ID_RADAR_ANIM: usize = 301;
pub const TIMER_ID_RADAR_LEASE: usize = 302;

pub const RADAR_ANIM_TICK_MS: u32 = 16;
pub const RADAR_LEASE_MS: u64 = 400;
const RADAR_LEASE_CHECK_MS: u32 = 140;

pub const RADAR_MIN_SIZE: (i32, i32) = (220, 200);
pub const RADAR_MAX_SIZE: (i32, i32) = (960, 800);

const MAX_TILE_LRU: usize = 32;
const MAX_ICON_LRU: usize = 64;
const MAX_UG_LRU: usize = 4;
const TELEPORT_SNAP_M: f64 = 45.0;
const POS_SPRING_OMEGA: f64 = 13.5;
const ROT_SMOOTH_RATE: f64 = 30.0;

/// Scale of HoYoLAB 1× CSS marker geometry (`42×45 px` native) inside the compact Radar overlay.
const RADAR_PIN_SCALE: f32 = 0.72;
const RADAR_WAYPOINT_SCALE: f32 = 0.64;

const _: () = assert!(WM_APP_RADAR_UPDATE != crate::layered::CAPTURE_AFFINITY_MSG);
const _: () = assert!(WM_APP_RADAR_HIDE != crate::layered::CAPTURE_AFFINITY_MSG);
const _: () = assert!(WM_APP_RADAR_EDIT != crate::layered::CAPTURE_AFFINITY_MSG);
const _: () = assert!(WM_APP_RADAR_ASSET_READY != crate::layered::CAPTURE_AFFINITY_MSG);
const _: () = assert!(TIMER_ID_RADAR_ANIM != crate::layered::TIMER_ID_WDA_REFRESH);
const _: () = assert!(TIMER_ID_RADAR_LEASE != crate::layered::TIMER_ID_WDA_REFRESH);

/// Compact pin entry parsed once from `/lan_pins` (zero JSON parsing on the 60 Hz render thread).
#[derive(Debug, Clone, PartialEq)]
pub struct RadarPin {
    pub id: String,
    pub lat: f64,
    pub lng: f64,
    pub label_id: i32,
    pub floor_id: Option<String>,
    pub marked: bool,
    pub is_underground: bool,
    pub icon_sign: i32,
    pub z_level: i32,
}

/// Default cyan RGBA color for legacy TSP route legs when no section color is provided.
pub const DEFAULT_RADAR_ROUTE_COLOR: [u8; 4] = [56, 189, 248, 228];

/// Route leg segment in world coordinates (`lat`/`lng` = `world_y`/`world_x`).
#[derive(Debug, Clone, PartialEq)]
pub struct RadarRouteLeg {
    pub from_lat: f64,
    pub from_lng: f64,
    pub to_lat: f64,
    pub to_lng: f64,
    pub done: bool,
    pub step_index: usize,
    /// Section stroke RGBA color (matches M-Map route overlay palette).
    pub color: [u8; 4],
    /// True for the currently active route section; false for secondary sections.
    pub emphasized: bool,
}

/// Active target, locked pin, or suggested route teleport highlighted on the Radar.
#[derive(Debug, Clone, PartialEq)]
pub struct RadarTargetHint {
    pub id: String,
    pub lat: f64,
    pub lng: f64,
    pub is_locked: bool,
    pub is_underground: bool,
    pub floor_id: Option<String>,
    /// True when this target originates from an active RouteDocument (stop or anchor teleport).
    pub is_route_stop: bool,
    /// Optional section accent RGBA used for the highlight ring (matches M-Map overlay).
    pub accent: Option<[u8; 4]>,
}

/// Immutable per-tick scene snapshot pushed from `tracking_loop` to `RadarMapController`.
#[derive(Debug, Clone)]
pub struct RadarScene {
    pub game_rect: RectI32,
    pub map_id: i32,
    pub world_x: f64,
    pub world_y: f64,
    pub vx: f64,
    pub vy: f64,
    pub body_heading: Option<f64>,
    pub camera_heading: Option<f64>,
    pub is_underground: bool,
    pub group_id: Option<i32>,
    pub floor_id: Option<i32>,
    pub floor_short: String,
    pub pins: Arc<Vec<RadarPin>>,
    pub excluded_ids: Arc<HashSet<String>>,
    pub target: Option<RadarTargetHint>,
    /// Suggested teleport waypoint for the active route section (highlighted just like M-Map).
    pub route_anchor: Option<RadarTargetHint>,
    pub route_legs: Arc<Vec<RadarRouteLeg>>,
    pub capture_visible: bool,
    pub edit_hint: String,
}

/// Callback invoked when the user finishes moving, resizing, or scrolling zoom/opacity in Edit Mode.
pub type RadarLayoutCallback =
    Arc<dyn Fn(Option<[f64; 4]>, f64, f64) + Send + Sync + 'static>;

/// Acyclic asset bridge implemented in `gh-app` over `HoyolabHdTileProvider`,
/// `UndergroundOverlayProvider`, `PinIconCache`, and `MultimapDb`.
pub trait RadarAssetSource: Send + Sync + 'static {
    fn maps_db(&self) -> Arc<MultimapDb>;
    fn tile_generation(&self) -> u64;
    fn icon_generation(&self) -> u64;
    fn get_color_tile_bytes(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> Option<Vec<u8>>;
    fn prefetch_color_tiles_async(&self, map_id: i32, tiles: &[(i32, i32, i32)]);
    fn get_ug_overlay_bytes(&self, map_id: i32, group_id: i32, floor_id: i32) -> Option<Vec<u8>>;
    fn get_icon_bytes_or_request(&self, map_id: i32, label_id: i32) -> Option<Vec<u8>>;
    fn get_marker_ui_asset_or_request(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Computes the default bottom-right Radar rectangle (landscape `~1.42:1`) matching the user's
/// reference placement: sits neatly below the 4-character party list and cleanly covers Genshin's
/// bottom-right `UID: XXXXXXXXX` watermark across 16:9 / 16:10 / 21:9 resolutions.
pub fn compute_default_radar_rect(game_rect: RectI32) -> RectI32 {
    let gw = game_rect.width.max(640) as f64;
    let gh = game_rect.height.max(480) as f64;
    let w = (gw * 0.24).round().clamp(340.0, 560.0) as i32;
    let h = (gh * 0.295).round().clamp(240.0, 395.0) as i32;
    let margin_r = ((gh / 1080.0) * 10.0).round().clamp(6.0, 18.0) as i32;
    let margin_b = ((gh / 1080.0) * 4.0).round().clamp(2.0, 10.0) as i32;
    RectI32 {
        x: game_rect.x + game_rect.width - w - margin_r,
        y: game_rect.y + game_rect.height - h - margin_b,
        width: w,
        height: h,
    }
}

/// Resolves screen-space Radar rect from optional relative `[x_rel, y_rel, w_rel, h_rel]`
/// or falls back to [`compute_default_radar_rect`], clamped within the monitor work area.
pub fn resolve_radar_rect(
    game_rect: RectI32,
    rel: Option<[f64; 4]>,
    work_area: Option<(i32, i32, i32, i32)>,
) -> RectI32 {
    let mut r = if let Some([rx, ry, rw, rh]) = rel {
        let gw = game_rect.width.max(320) as f64;
        let gh = game_rect.height.max(240) as f64;
        let w = (rw * gw).round() as i32;
        let h = (rh * gh).round() as i32;
        let w = w.clamp(RADAR_MIN_SIZE.0, RADAR_MAX_SIZE.0);
        let h = h.clamp(RADAR_MIN_SIZE.1, RADAR_MAX_SIZE.1);
        let x = game_rect.x + (rx * gw).round() as i32;
        let y = game_rect.y + (ry * gh).round() as i32;
        RectI32 {
            x,
            y,
            width: w,
            height: h,
        }
    } else {
        compute_default_radar_rect(game_rect)
    };

    if let Some((wl, wt, wr, wb)) = work_area
        && wr > wl
        && wb > wt
    {
        r.width = r.width.clamp(RADAR_MIN_SIZE.0, (wr - wl).min(RADAR_MAX_SIZE.0));
        r.height = r.height.clamp(RADAR_MIN_SIZE.1, (wb - wt).min(RADAR_MAX_SIZE.1));
        r.x = r.x.clamp(wl, (wr - r.width).max(wl));
        r.y = r.y.clamp(wt, (wb - r.height).max(wt));
    }
    r
}

/// Converts a screen-space Radar rect back to relative `[x_rel, y_rel, w_rel, h_rel]` inside `game_rect`.
pub fn radar_rect_to_relative(window_rect: RectI32, game_rect: RectI32) -> Option<[f64; 4]> {
    if game_rect.width <= 0 || game_rect.height <= 0 || window_rect.width <= 0 || window_rect.height <= 0 {
        return None;
    }
    let gw = game_rect.width as f64;
    let gh = game_rect.height as f64;
    let rx = ((window_rect.x - game_rect.x) as f64 / gw).clamp(0.0, 0.98);
    let ry = ((window_rect.y - game_rect.y) as f64 / gh).clamp(0.0, 0.98);
    let rw = (window_rect.width as f64 / gw).clamp(0.05, 0.85);
    let rh = (window_rect.height as f64 / gh).clamp(0.05, 0.85);
    Some([
        (rx * 10_000.0).round() / 10_000.0,
        (ry * 10_000.0).round() / 10_000.0,
        (rw * 10_000.0).round() / 10_000.0,
        (rh * 10_000.0).round() / 10_000.0,
    ])
}

/// Returns true if `radar_rect` intersects the game's top-left minimap or Paimon CV zones
/// (padded by `16 px`), in which case `WDA_EXCLUDEFROMCAPTURE` MUST be forced on.
pub fn radar_overlaps_cv_zones(radar_rect: RectI32, game_rect: RectI32) -> bool {
    if game_rect.width <= 0 || game_rect.height <= 0 {
        return true;
    }
    let Ok(layout) = compute_ui_layout(game_rect.width as u32, game_rect.height as u32, false)
    else {
        return true;
    };
    let pad = 16;
    let zones = [layout.minimap_rect, layout.paimon_rect];
    for z in zones {
        let zx0 = game_rect.x + z.x - pad;
        let zy0 = game_rect.y + z.y - pad;
        let zx1 = game_rect.x + z.x + z.width + pad;
        let zy1 = game_rect.y + z.y + z.height + pad;
        let rx0 = radar_rect.x;
        let ry0 = radar_rect.y;
        let rx1 = radar_rect.x + radar_rect.width;
        let ry1 = radar_rect.y + radar_rect.height;
        if rx0 < zx1 && rx1 > zx0 && ry0 < zy1 && ry1 > zy0 {
            return true;
        }
    }
    false
}

/// North-Up world-to-screen projection (exact match with HoYoLAB / LAN Companion).
#[inline]
pub fn world_to_radar_px(
    wx: f64,
    wy: f64,
    center_wx: f64,
    center_wy: f64,
    zoom: f64,
    cx: f32,
    cy: f32,
) -> (f32, f32) {
    let s = 2f64.powf(zoom);
    let sx = cx as f64 + (wx - center_wx) * s;
    let sy = cy as f64 + (wy - center_wy) * s;
    (sx as f32, sy as f32)
}

/// Transforms an underground `FloorOverlayRecord` onto Radar pixel space, returning
/// `(dst_x, dst_y, dst_w, dst_h, reflect_y)`.
pub fn underground_overlay_screen_box(
    rec: &FloorOverlayRecord,
    center_wx: f64,
    center_wy: f64,
    zoom: f64,
    cx: f32,
    cy: f32,
) -> Option<(f32, f32, f32, f32, bool)> {
    let (min_lng, max_lng, min_lat, max_lat) = rec.overlay.bounds();
    if !(min_lng.is_finite()
        && max_lng.is_finite()
        && min_lat.is_finite()
        && max_lat.is_finite())
        || max_lng <= min_lng
        || max_lat <= min_lat
    {
        return None;
    }
    let (x0, y0) = world_to_radar_px(min_lng, min_lat, center_wx, center_wy, zoom, cx, cy);
    let (x1, y1) = world_to_radar_px(max_lng, max_lat, center_wx, center_wy, zoom, cx, cy);
    let dx = x0.min(x1);
    let dy = y0.min(y1);
    let dw = (x1 - x0).abs();
    let dh = (y1 - y0).abs();
    if dw < 1.0 || dh < 1.0 {
        return None;
    }
    Some((dx, dy, dw, dh, rec.overlay.reflect_y.unwrap_or(false)))
}

// ---------------------------------------------------------------------------
// Bounded Premultiplied Pixmap LRU & Async Background Decoder
// ---------------------------------------------------------------------------

struct LruMap<K: Eq + std::hash::Hash + Copy, V> {
    cap: usize,
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Eq + std::hash::Hash + Copy, V: Clone> LruMap<K, V> {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            entries: HashMap::with_capacity(cap.max(1)),
            order: VecDeque::with_capacity(cap.max(1)),
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let val = self.entries.get(key)?.clone();
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(*key);
        Some(val)
    }

    fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    fn insert(&mut self, key: K, value: V) {
        if let Some(pos) = self.order.iter().position(|k| k == &key) {
            self.order.remove(pos);
        }
        self.entries.insert(key, value);
        self.order.push_back(key);
        while self.entries.len() > self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

/// Official HoYoLAB Interactive Map marker UI sprite kinds (fetched at runtime from official
/// HoYoLAB/HoYoVerse CDN via `RadarAssetSource::get_marker_ui_asset_or_request`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MarkerUiKind {
    BgNormal,
    BgSelected,
    SelectedFrame,
    UndergroundMark,
    UndergroundMarkSelected,
    WaterholeMark,
    WaterholeMarkSelected,
    ZLevelUpper,
    ZLevelMiddle,
    ZLevelLower,
}

impl MarkerUiKind {
    const ALL: &'static [MarkerUiKind] = &[
        Self::BgNormal,
        Self::BgSelected,
        Self::SelectedFrame,
        Self::UndergroundMark,
        Self::UndergroundMarkSelected,
        Self::WaterholeMark,
        Self::WaterholeMarkSelected,
        Self::ZLevelUpper,
        Self::ZLevelMiddle,
        Self::ZLevelLower,
    ];

    const fn key(self) -> &'static str {
        match self {
            Self::BgNormal => "marker_bg_normal",
            Self::BgSelected => "marker_bg_selected",
            Self::SelectedFrame => "marker_selected_frame",
            Self::UndergroundMark => "marker_ug_mark",
            Self::UndergroundMarkSelected => "marker_ug_mark_selected",
            Self::WaterholeMark => "marker_wh_mark",
            Self::WaterholeMarkSelected => "marker_wh_mark_selected",
            Self::ZLevelUpper => "marker_z_upper",
            Self::ZLevelMiddle => "marker_z_middle",
            Self::ZLevelLower => "marker_z_lower",
        }
    }
}

struct DecodedAssetBank {
    tiles: LruMap<(i32, i32, i32, i32), Arc<Pixmap>>,
    icons: LruMap<(i32, i32), Arc<Pixmap>>,
    ui_sprites: HashMap<MarkerUiKind, Arc<Pixmap>>,
    ug: LruMap<(i32, i32, i32), Arc<Pixmap>>,
    pending_tiles: HashSet<(i32, i32, i32, i32)>,
    pending_icons: HashSet<(i32, i32)>,
    pending_ui: HashSet<MarkerUiKind>,
    pending_ug: HashSet<(i32, i32, i32)>,
    icon_miss_gen: HashMap<(i32, i32), u64>,
    ui_miss_gen: HashMap<MarkerUiKind, u64>,
}

impl DecodedAssetBank {
    fn new() -> Self {
        Self {
            tiles: LruMap::new(MAX_TILE_LRU),
            icons: LruMap::new(MAX_ICON_LRU),
            ui_sprites: HashMap::with_capacity(MarkerUiKind::ALL.len()),
            ug: LruMap::new(MAX_UG_LRU),
            pending_tiles: HashSet::new(),
            pending_icons: HashSet::new(),
            pending_ui: HashSet::new(),
            pending_ug: HashSet::new(),
            icon_miss_gen: HashMap::new(),
            ui_miss_gen: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.tiles.clear();
        self.icons.clear();
        self.ui_sprites.clear();
        self.ug.clear();
        self.pending_tiles.clear();
        self.pending_icons.clear();
        self.pending_ui.clear();
        self.pending_ug.clear();
        self.icon_miss_gen.clear();
        self.ui_miss_gen.clear();
    }
}

enum DecodeReq {
    Tile(i32, i32, i32, i32),
    Icon(i32, i32, u64),
    UiSprite(MarkerUiKind, u64),
    Ug(i32, i32, i32),
    Shutdown,
}

fn rgba_image_to_pixmap(rgba: &image::RgbaImage) -> Option<Pixmap> {
    let w = rgba.width();
    let h = rgba.height();
    let mut pix = Pixmap::new(w, h)?;
    let dst = pix.data_mut();
    for (src_px, dst_px) in rgba
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
    {
        let a = src_px[3] as u16;
        dst_px[0] = ((src_px[0] as u16 * a + 127) / 255) as u8;
        dst_px[1] = ((src_px[1] as u16 * a + 127) / 255) as u8;
        dst_px[2] = ((src_px[2] as u16 * a + 127) / 255) as u8;
        dst_px[3] = src_px[3];
    }
    Some(pix)
}

fn decode_image_bytes_to_pixmap(bytes: &[u8]) -> Option<Arc<Pixmap>> {
    let dyn_img = image::load_from_memory(bytes).ok()?;
    let rgba = dyn_img.to_rgba8();
    rgba_image_to_pixmap(&rgba).map(Arc::new)
}

/// Decodes an official HoYoLAB category/waypoint icon PNG into a `48×48` 2×-supersampled
/// anti-aliased circular-clipped premultiplied `Pixmap` matching HoYoLAB `ciLayer._drawImage`.
fn decode_circular_category_icon(icon_bytes: &[u8]) -> Option<Arc<Pixmap>> {
    const SZ: u32 = 48;
    let dyn_img = image::load_from_memory(icon_bytes).ok()?;
    let resized = dyn_img
        .resize_exact(SZ, SZ, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let src_pm = rgba_image_to_pixmap(&resized)?;

    let mut mask = Mask::new(SZ, SZ)?;
    let c = SZ as f32 * 0.5;
    if let Some(circle) = PathBuilder::from_circle(c, c, c - 0.8) {
        mask.fill_path(&circle, FillRule::Winding, true, Transform::identity());
    }
    let mut out = Pixmap::new(SZ, SZ)?;
    let paint = PixmapPaint {
        opacity: 1.0,
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(0, 0, src_pm.as_ref(), &paint, Transform::identity(), Some(&mask));
    Some(Arc::new(out))
}

/// Analytic 1D critically-damped spring step (`C¹` velocity-continuous, zero overshoot).
#[inline]
pub(crate) fn step_critically_damped_axis(
    pos: f64,
    vel: f64,
    target: f64,
    omega: f64,
    dt: f64,
) -> (f64, f64) {
    let x = pos - target;
    let exp = (-omega * dt).exp();
    let new_pos = target + (x + (vel + omega * x) * dt) * exp;
    let new_vel = (vel - (vel + omega * x) * omega * dt) * exp;
    (new_pos, new_vel)
}

// ---------------------------------------------------------------------------
// Shared State Between Controller API, UI Thread, and Decoder Thread
// ---------------------------------------------------------------------------

struct RadarShared {
    scene: Option<RadarScene>,
    last_scene_at: Option<Instant>,
    edit_mode: bool,
    rel_rect: Option<[f64; 4]>,
    zoom: f64,
    opacity: f64,
    capture_visible: bool,
    last_tile_gen: u64,
    last_icon_gen: u64,
}

struct RadarUiState {
    shared: Arc<Mutex<RadarShared>>,
    assets: Arc<dyn RadarAssetSource>,
    bank: Arc<Mutex<DecodedAssetBank>>,
    decode_tx: mpsc::SyncSender<DecodeReq>,
    on_layout_saved: Option<RadarLayoutCallback>,
    fonts: HudFonts,
    // Reusable zero-alloc render buffers
    pixmap: Option<Pixmap>,
    clip_mask: Option<Mask>,
    clip_mask_size: (u32, u32),
    // 60 Hz smoother state (C¹ critically-damped position spring + snappy rotation EMA)
    smooth_wx: Option<f64>,
    smooth_wy: Option<f64>,
    smooth_vx: f64,
    smooth_vy: f64,
    smooth_body_deg: Option<f64>,
    smooth_cam_deg: Option<f64>,
    last_map_id: Option<i32>,
    last_anim_ts: Option<Instant>,
    anim_timer_active: bool,
    lease_timer_active: bool,
    hover_fade: HoverFadeController,
    last_presented_rect: Option<RectI32>,
    last_forced_wda_exclude: Option<bool>,
    last_effective_alpha: u8,
    live_interaction_rect: Option<RectI32>,
    // Tile prefetch throttle
    last_prefetch_ts: Option<Instant>,
    /// When `true` (`HeadlessRadarRenderer`), asset decodes run synchronously inline with zero
    /// background threads and zero Win32 HWND. Live `RadarMapController` keeps this `false`.
    sync_decode: bool,
    sync_decoded_any: AtomicBool,
}

impl RadarUiState {
    fn queue_tile_decode(&self, map_id: i32, z: i32, tx: i32, ty: i32) {
        let key = (map_id, z, tx, ty);
        if self.sync_decode {
            let already = {
                let b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                b.tiles.contains(&key)
            };
            if !already
                && let Some(pm) = self
                    .assets
                    .get_color_tile_bytes(map_id, z, tx, ty)
                    .and_then(|bytes| decode_image_bytes_to_pixmap(&bytes))
            {
                let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                b.tiles.insert(key, pm);
                self.sync_decoded_any.store(true, Ordering::Relaxed);
            }
            return;
        }
        let should_send = {
            let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            if b.tiles.contains(&key) || b.pending_tiles.contains(&key) {
                false
            } else {
                b.pending_tiles.insert(key);
                true
            }
        };
        if should_send && self.decode_tx.try_send(DecodeReq::Tile(map_id, z, tx, ty)).is_err() {
            let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            b.pending_tiles.remove(&key);
        }
    }

    fn queue_icon_decode(&self, map_id: i32, label_id: i32, icon_gen: u64) {
        if label_id <= 0 {
            return;
        }
        let key = (map_id, label_id);
        if self.sync_decode {
            let skip = {
                let b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                b.icons.contains(&key) || b.icon_miss_gen.get(&key).copied() == Some(icon_gen)
            };
            if !skip {
                let decoded = self
                    .assets
                    .get_icon_bytes_or_request(map_id, label_id)
                    .and_then(|bytes| decode_image_bytes_to_pixmap(&bytes));
                let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(pm) = decoded {
                    b.icon_miss_gen.remove(&key);
                    b.icons.insert(key, pm);
                    self.sync_decoded_any.store(true, Ordering::Relaxed);
                } else {
                    b.icon_miss_gen.insert(key, icon_gen);
                }
            }
            return;
        }
        let should_send = {
            let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            if b.icons.contains(&key)
                || b.pending_icons.contains(&key)
                || b.icon_miss_gen.get(&key).copied() == Some(icon_gen)
            {
                false
            } else {
                b.pending_icons.insert(key);
                true
            }
        };
        if should_send
            && self
                .decode_tx
                .try_send(DecodeReq::Icon(map_id, label_id, icon_gen))
                .is_err()
        {
            let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            b.pending_icons.remove(&key);
        }
    }

    fn ensure_ui_sprites_queued(&self, icon_gen: u64) {
        for &kind in MarkerUiKind::ALL {
            if self.sync_decode {
                let skip = {
                    let b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                    b.ui_sprites.contains_key(&kind)
                        || b.ui_miss_gen.get(&kind).copied() == Some(icon_gen)
                };
                if !skip {
                    let decoded = self
                        .assets
                        .get_marker_ui_asset_or_request(kind.key())
                        .and_then(|bytes| decode_image_bytes_to_pixmap(&bytes));
                    let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(pm) = decoded {
                        b.ui_miss_gen.remove(&kind);
                        b.ui_sprites.insert(kind, pm);
                        self.sync_decoded_any.store(true, Ordering::Relaxed);
                    } else {
                        b.ui_miss_gen.insert(kind, icon_gen);
                    }
                }
                continue;
            }
            let should_send = {
                let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                if b.ui_sprites.contains_key(&kind)
                    || b.pending_ui.contains(&kind)
                    || b.ui_miss_gen.get(&kind).copied() == Some(icon_gen)
                {
                    false
                } else {
                    b.pending_ui.insert(kind);
                    true
                }
            };
            if should_send
                && self
                    .decode_tx
                    .try_send(DecodeReq::UiSprite(kind, icon_gen))
                    .is_err()
            {
                let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                b.pending_ui.remove(&kind);
            }
        }
    }

    fn queue_ug_decode(&self, map_id: i32, group_id: i32, floor_id: i32) {
        let key = (map_id, group_id, floor_id);
        if self.sync_decode {
            let already = {
                let b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                b.ug.contains(&key)
            };
            if !already
                && let Some(pm) = self
                    .assets
                    .get_ug_overlay_bytes(map_id, group_id, floor_id)
                    .and_then(|bytes| decode_image_bytes_to_pixmap(&bytes))
            {
                let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                b.ug.insert(key, pm);
                self.sync_decoded_any.store(true, Ordering::Relaxed);
            }
            return;
        }
        let should_send = {
            let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            if b.ug.contains(&key) || b.pending_ug.contains(&key) {
                false
            } else {
                b.pending_ug.insert(key);
                true
            }
        };
        if should_send && self.decode_tx.try_send(DecodeReq::Ug(map_id, group_id, floor_id)).is_err() {
            let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            b.pending_ug.remove(&key);
        }
    }

    fn ensure_anim_timer(&mut self, ctx: &LayeredWindowCtx<'_>, want: bool) {
        if want && !self.anim_timer_active {
            ctx.set_timer(TIMER_ID_RADAR_ANIM, RADAR_ANIM_TICK_MS);
            self.anim_timer_active = true;
        } else if !want && self.anim_timer_active {
            ctx.kill_timer(TIMER_ID_RADAR_ANIM);
            self.anim_timer_active = false;
            self.last_anim_ts = None;
        }
    }

    fn ensure_lease_timer(&mut self, ctx: &LayeredWindowCtx<'_>, want: bool) {
        if want && !self.lease_timer_active {
            ctx.set_timer(TIMER_ID_RADAR_LEASE, RADAR_LEASE_CHECK_MS);
            self.lease_timer_active = true;
        } else if !want && self.lease_timer_active {
            ctx.kill_timer(TIMER_ID_RADAR_LEASE);
            self.lease_timer_active = false;
        }
    }

    fn hide_clean(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        self.ensure_anim_timer(ctx, false);
        self.ensure_lease_timer(ctx, false);
        self.live_interaction_rect = None;
        ctx.hide();
    }

    fn step_and_render(&mut self, ctx: &mut LayeredWindowCtx<'_>, force_redraw: bool) {
        let (scene_opt, edit_mode, rel_rect, zoom, base_opacity, want_cap, last_at) = {
            let g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (
                g.scene.clone(),
                g.edit_mode,
                g.rel_rect,
                g.zoom,
                g.opacity,
                g.capture_visible,
                g.last_scene_at,
            )
        };

        if ctx.is_edit_mode() != edit_mode {
            ctx.set_edit_mode(edit_mode);
        }

        let Some(scene) = scene_opt else {
            if !edit_mode {
                self.hide_clean(ctx);
            }
            return;
        };

        if !edit_mode
            && last_at.is_some_and(|t0| t0.elapsed() > Duration::from_millis(RADAR_LEASE_MS))
        {
            self.hide_clean(ctx);
            return;
        }

        // Resolve target window rect (live direct drag/resize ALWAYS overrides persisted rel_rect).
        // Use full monitor bounds (`rcMonitor`) rather than `rcWork` so Borderless Fullscreen
        // players can drag/size the Radar all the way to the bottom edge over the UID watermark.
        let bounds = query_monitor_bounds(
            scene.game_rect.x + scene.game_rect.width / 2,
            scene.game_rect.y + scene.game_rect.height / 2,
        );
        let rect = if let Some(live) = self.live_interaction_rect {
            live
        } else {
            resolve_radar_rect(scene.game_rect, rel_rect, Some(bounds))
        };

        // CV Safety Guard: force WDA_EXCLUDEFROMCAPTURE whenever Radar overlaps top-left minimap/Paimon.
        let eff_cap_visible = want_cap && !radar_overlaps_cv_zones(rect, scene.game_rect);
        let force_exclude = !eff_cap_visible;
        if self.last_forced_wda_exclude != Some(force_exclude) {
            ctx.set_capture_visible(eff_cap_visible);
            self.last_forced_wda_exclude = Some(force_exclude);
        }

        // Step 60 Hz frame-rate-independent C¹ critically-damped position spring & rotation EMA.
        let now = Instant::now();
        let dt = self
            .last_anim_ts
            .map(|prev| now.duration_since(prev).as_secs_f64().clamp(0.001, 0.1))
            .unwrap_or(1.0 / 60.0);
        self.last_anim_ts = Some(now);

        let jumped = self.last_map_id != Some(scene.map_id)
            || self
                .smooth_wx
                .zip(self.smooth_wy)
                .is_none_or(|(px, py)| (scene.world_x - px).hypot(scene.world_y - py) > TELEPORT_SNAP_M);

        let scale_px_per_world = 2f64.powf(zoom).max(0.1);
        if jumped {
            self.smooth_wx = Some(scene.world_x);
            self.smooth_wy = Some(scene.world_y);
            self.smooth_vx = 0.0;
            self.smooth_vy = 0.0;
            self.smooth_body_deg = scene.body_heading.or(scene.camera_heading);
            self.smooth_cam_deg = scene.camera_heading.or(scene.body_heading);
            self.last_map_id = Some(scene.map_id);
        } else {
            let alpha_rot = 1.0 - (-dt * ROT_SMOOTH_RATE).exp();
            if let (Some(px), Some(py)) = (self.smooth_wx, self.smooth_wy) {
                let err_px = (scene.world_x - px).hypot(scene.world_y - py) * scale_px_per_world;
                let speed_px = self.smooth_vx.hypot(self.smooth_vy) * scale_px_per_world;
                if err_px < 0.22 && speed_px < 1.5 {
                    // Micro-deadband when effectively stationary: keeps pins rock-solid.
                    self.smooth_vx = 0.0;
                    self.smooth_vy = 0.0;
                } else {
                    let (nx, nvx) = step_critically_damped_axis(
                        px,
                        self.smooth_vx,
                        scene.world_x,
                        POS_SPRING_OMEGA,
                        dt,
                    );
                    let (ny, nvy) = step_critically_damped_axis(
                        py,
                        self.smooth_vy,
                        scene.world_y,
                        POS_SPRING_OMEGA,
                        dt,
                    );
                    self.smooth_wx = Some(nx);
                    self.smooth_wy = Some(ny);
                    self.smooth_vx = nvx;
                    self.smooth_vy = nvy;
                }
            }
            if let Some(target_body) = scene.body_heading.or(scene.camera_heading) {
                let cur = self.smooth_body_deg.unwrap_or(target_body);
                let delta = wrap_deg_signed(target_body - cur);
                self.smooth_body_deg = Some(wrap_deg_signed(cur + delta * alpha_rot));
            }
            if let Some(target_cam) = scene.camera_heading.or(scene.body_heading) {
                let cur = self.smooth_cam_deg.unwrap_or(target_cam);
                let delta = wrap_deg_signed(target_cam - cur);
                self.smooth_cam_deg = Some(wrap_deg_signed(cur + delta * alpha_rot));
            }
        }

        let rx = self.smooth_wx.unwrap_or(scene.world_x);
        let ry = self.smooth_wy.unwrap_or(scene.world_y);
        let body_deg = self.smooth_body_deg.unwrap_or(0.0);
        let cam_deg = self.smooth_cam_deg.unwrap_or(body_deg);

        // Hover auto-fade when player holds Alt & hovers over Radar outside Edit Mode.
        #[cfg(windows)]
        let cursor_pos = {
            let mut pt = POINT::default();
            // SAFETY: Plain Win32 read-only cursor query.
            if unsafe { GetCursorPos(&mut pt).is_ok() } {
                Some((pt.x, pt.y))
            } else {
                None
            }
        };
        #[cfg(not(windows))]
        let cursor_pos = gh_platform::capture::get_cursor_pos();
        let hovered = cursor_pos.is_some_and(|(px, py)| {
            px >= rect.x
                && px < rect.x.saturating_add(rect.width)
                && py >= rect.y
                && py < rect.y.saturating_add(rect.height)
        });
        let fade_suppressed = edit_mode || ctx.is_dragging();
        let hover_changed = self.hover_fade.tick(hovered, fade_suppressed).is_some();
        let hover_alpha_u8 = self.hover_fade.current_alpha();
        let hover_animating = hover_changed
            || (hovered && !fade_suppressed && hover_alpha_u8 != self.hover_fade.faded_alpha())
            || ((!hovered || fade_suppressed) && hover_alpha_u8 != self.hover_fade.full_alpha());
        let effective_alpha = ((hover_alpha_u8 as f64 / 255.0) * base_opacity * 255.0)
            .round()
            .clamp(30.0, 255.0) as u8;
        self.last_effective_alpha = effective_alpha;

        // Check whether new disk tiles/icons arrived since last frame.
        let cur_tile_gen = self.assets.tile_generation();
        let cur_icon_gen = self.assets.icon_generation();
        let assets_bumped = {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            let changed = g.last_tile_gen != cur_tile_gen || g.last_icon_gen != cur_icon_gen;
            g.last_tile_gen = cur_tile_gen;
            g.last_icon_gen = cur_icon_gen;
            changed
        };

        let pos_err_px = (scene.world_x - rx).hypot(scene.world_y - ry) * scale_px_per_world;
        let speed_px = self.smooth_vx.hypot(self.smooth_vy) * scale_px_per_world;
        let body_err = scene
            .body_heading
            .or(scene.camera_heading)
            .map(|t| wrap_deg_signed(t - body_deg).abs())
            .unwrap_or(0.0);
        let cam_err = scene
            .camera_heading
            .or(scene.body_heading)
            .map(|t| wrap_deg_signed(t - cam_deg).abs())
            .unwrap_or(0.0);
        let still_animating =
            pos_err_px > 0.25 || speed_px > 1.5 || body_err > 0.15 || cam_err > 0.15 || hover_animating;

        let rect_changed = self.last_presented_rect != Some(rect);
        let need_draw = force_redraw || rect_changed || still_animating || assets_bumped || !ctx.is_visible();

        if need_draw {
            let w = rect.width.max(32) as u32;
            let h = rect.height.max(32) as u32;
            self.render_frame_into_pixmap(w, h, &scene, rx, ry, body_deg, cam_deg, zoom, edit_mode);
            let keep_pos = ctx.is_dragging() && self.live_interaction_rect.is_none();
            if let Some(ref pm) = self.pixmap
                && ctx
                    .present_premultiplied_rgba(
                        rect.x,
                        rect.y,
                        w as i32,
                        h as i32,
                        pm.data(),
                        effective_alpha,
                        keep_pos,
                    )
                    .is_ok()
            {
                self.last_presented_rect = Some(rect);
            }
        } else if ctx.is_visible() {
            let _ = ctx.present_alpha(effective_alpha);
        }

        // Dynamic timers: 0% CPU when converged & stationary!
        self.ensure_anim_timer(ctx, still_animating || assets_bumped);
        self.ensure_lease_timer(ctx, !edit_mode);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_frame_into_pixmap(
        &mut self,
        w: u32,
        h: u32,
        scene: &RadarScene,
        rx: f64,
        ry: f64,
        body_deg: f64,
        cam_deg: f64,
        zoom: f64,
        edit_mode: bool,
    ) {
        let realloc = self
            .pixmap
            .as_ref()
            .is_none_or(|p| p.width() != w || p.height() != h);
        if realloc {
            self.pixmap = Pixmap::new(w, h);
        }
        let Some(mut pm) = self.pixmap.take() else {
            return;
        };
        pm.fill(Color::TRANSPARENT);

        let wf = w as f32;
        let hf = h as f32;
        let dpi_scale = (scene.game_rect.height as f32 / 1080.0).clamp(0.75, 2.0);
        let pad = 2.0f32;
        let radius = (12.0 * dpi_scale).clamp(8.0, 18.0);

        // Cached rounded-rectangle clip mask for the map interior.
        if self.clip_mask_size != (w, h) || self.clip_mask.is_none() {
            let mut m = Mask::new(w, h);
            if let Some(ref mut mask) = m
                && let Some(path) = build_rounded_rect_path(
                    pad,
                    pad,
                    (wf - pad * 2.0).max(8.0),
                    (hf - pad * 2.0).max(8.0),
                    radius,
                )
            {
                mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
            }
            self.clip_mask = m;
            self.clip_mask_size = (w, h);
        }

        // 1. Soft outer drop shadow feather (separates cleanly from bright snow/sand).
        if let Some(shadow_path) = build_rounded_rect_path(0.5, 1.0, wf - 1.0, hf - 1.5, radius + 1.0)
        {
            let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
            p.set_color_rgba8(6, 9, 16, 115);
            pm.fill_path(&shadow_path, &p, FillRule::Winding, Transform::identity(), None);
        }

        // 2. Deep Teyvat slate ocean backdrop inside the rounded card.
        if let Some(bg_path) = build_rounded_rect_path(pad, pad, wf - pad * 2.0, hf - pad * 2.0, radius)
        {
            let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
            p.set_color_rgba8(20, 26, 38, 246);
            pm.fill_path(&bg_path, &p, FillRule::Winding, Transform::identity(), None);
        }

        let cx = wf * 0.5;
        let cy = hf * 0.5;
        let scale_px_per_world = 2f64.powf(zoom);

        // 3. Draw North-Up HoYoLAB Color Tiles with zero-blank parent fallback.
        let maps_db = self.assets.maps_db();
        let mut tiles_to_prefetch: Vec<(i32, i32, i32)> = Vec::new();
        if let Some(mdata) = maps_db.map(scene.map_id)
            && let Some(ref d2) = mdata.detail_v2
            && !d2.map_version.trim().is_empty()
        {
            let tz = (zoom.ceil() as i32).clamp(d2.min_zoom, d2.max_zoom);
            let tz_scale = 2f64.powi(tz);
            let tile_world = 256.0 / tz_scale;
            let half_w_world = (wf as f64 * 0.5) / scale_px_per_world;
            let half_h_world = (hf as f64 * 0.5) / scale_px_per_world;
            let (tx0, tx1, ty0, ty1) = d2.tile_range_for_world_rect(
                rx - half_w_world,
                rx + half_w_world,
                ry - half_h_world,
                ry + half_h_world,
                tz,
            );
            let (max_tx, max_ty) = d2.max_tile_xy(tz);
            let ox = d2.origin[0];
            let oy = d2.origin[1];
            let dst_tile_px = (tile_world * scale_px_per_world) as f32;

            let paint = PixmapPaint {
                opacity: 1.0,
                quality: FilterQuality::Bilinear,
                ..PixmapPaint::default()
            };

            for ty in ty0..=ty1.min(max_ty) {
                for tx in tx0..=tx1.min(max_tx) {
                    let tile_wx = (tx as f64 * 256.0) / tz_scale - ox;
                    let tile_wy = (ty as f64 * 256.0) / tz_scale - oy;
                    let (sx, sy) = world_to_radar_px(tile_wx, tile_wy, rx, ry, zoom, cx, cy);

                    let exact = {
                        let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                        b.tiles.get(&(scene.map_id, tz, tx, ty))
                    };
                    if let Some(tile_pm) = exact {
                        let sx_scale = dst_tile_px / tile_pm.width().max(1) as f32;
                        let sy_scale = dst_tile_px / tile_pm.height().max(1) as f32;
                        let tf = Transform::from_scale(sx_scale, sy_scale).post_translate(sx, sy);
                        pm.draw_pixmap(0, 0, (*tile_pm).as_ref(), &paint, tf, self.clip_mask.as_ref());
                    } else {
                        // Queue async decode/download of exact tile & draw parent fallback cleanly.
                        self.queue_tile_decode(scene.map_id, tz, tx, ty);
                        let mut drew_parent = false;
                        for pz in (d2.min_zoom..tz).rev() {
                            let shift = (tz - pz) as u32;
                            let ptx = tx >> shift;
                            let pty = ty >> shift;
                            let parent_opt = {
                                let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                                b.tiles.get(&(scene.map_id, pz, ptx, pty))
                            };
                            if let Some(parent_pm) = parent_opt {
                                let factor = (1u32 << shift) as f32;
                                let sub_w = parent_pm.width() as f32 / factor;
                                let sub_h = parent_pm.height() as f32 / factor;
                                let sub_x = (tx - (ptx << shift)) as f32 * sub_w;
                                let sub_y = (ty - (pty << shift)) as f32 * sub_h;
                                let sx_scale = dst_tile_px / sub_w.max(1.0);
                                let sy_scale = dst_tile_px / sub_h.max(1.0);
                                // Crop sub-tile by clipping to the destination tile rect (tiny-skia).
                                let mut sub_mask = Mask::new(w, h);
                                if let Some(ref mut sm) = sub_mask
                                    && let Some(tile_rect_path) =
                                        build_rounded_rect_path(sx, sy, dst_tile_px + 0.6, dst_tile_px + 0.6, 0.0)
                                {
                                    sm.fill_path(
                                        &tile_rect_path,
                                        FillRule::Winding,
                                        false,
                                        Transform::identity(),
                                    );
                                    if let Some(ref outer) = self.clip_mask {
                                        for (m_px, o_px) in sm.data_mut().iter_mut().zip(outer.data().iter()) {
                                            *m_px = ((*m_px as u16 * *o_px as u16) / 255) as u8;
                                        }
                                    }
                                    let tf = Transform::from_translate(-sub_x, -sub_y)
                                        .post_scale(sx_scale, sy_scale)
                                        .post_translate(sx, sy);
                                    pm.draw_pixmap(0, 0, (*parent_pm).as_ref(), &paint, tf, Some(sm));
                                    drew_parent = true;
                                }
                                break;
                            } else if pz == d2.min_zoom {
                                self.queue_tile_decode(scene.map_id, pz, ptx, pty);
                            }
                        }
                        let _ = drew_parent;
                    }
                }
            }

            // Velocity-ahead predictive ring prefetch (throttled to ~2 Hz).
            if self
                .last_prefetch_ts
                .is_none_or(|t0| t0.elapsed() >= Duration::from_millis(450))
            {
                self.last_prefetch_ts = Some(Instant::now());
                let lead_wx = rx + scene.vx.clamp(-80.0, 80.0) * 1.4;
                let lead_wy = ry + scene.vy.clamp(-80.0, 80.0) * 1.4;
                let (ptx0, ptx1, pty0, pty1) = d2.tile_range_for_world_rect(
                    lead_wx - half_w_world * 1.35,
                    lead_wx + half_w_world * 1.35,
                    lead_wy - half_h_world * 1.35,
                    lead_wy + half_h_world * 1.35,
                    tz,
                );
                for py in pty0..=pty1.min(max_ty) {
                    for px in ptx0..=ptx1.min(max_tx) {
                        tiles_to_prefetch.push((tz, px, py));
                    }
                }
            }
        }
        if !tiles_to_prefetch.is_empty() {
            self.assets
                .prefetch_color_tiles_async(scene.map_id, &tiles_to_prefetch);
        }

        // 4. Underground floor overlay (dim surface 0.56 + honor `reflect_y`).
        if scene.is_underground {
            if let Some(dim_path) =
                build_rounded_rect_path(pad, pad, wf - pad * 2.0, hf - pad * 2.0, radius)
            {
                let mut p = Paint::default();
                p.set_color_rgba8(10, 12, 18, 143); // rgba(10,12,18,0.56)
                pm.fill_path(&dim_path, &p, FillRule::Winding, Transform::identity(), self.clip_mask.as_ref());
            }
            if let Some(rec) = resolve_active_floor_overlay(&maps_db, scene, rx, ry) {
                let ug_pm = {
                    let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                    b.ug.get(&(rec.map_id, rec.group_id, rec.floor_id))
                };
                if let Some(ug_img) = ug_pm {
                    if let Some((dx, dy, dw, dh, reflect_y)) =
                        underground_overlay_screen_box(&rec, rx, ry, zoom, cx, cy)
                    {
                        let sx = dw / ug_img.width().max(1) as f32;
                        let sy = dh / ug_img.height().max(1) as f32;
                        let tf = if reflect_y {
                            Transform::from_scale(sx, -sy).post_translate(dx, dy + dh)
                        } else {
                            Transform::from_scale(sx, sy).post_translate(dx, dy)
                        };
                        let paint = PixmapPaint {
                            opacity: 0.96,
                            quality: FilterQuality::Bilinear,
                            ..PixmapPaint::default()
                        };
                        pm.draw_pixmap(0, 0, (*ug_img).as_ref(), &paint, tf, self.clip_mask.as_ref());
                    }
                } else {
                    self.queue_ug_decode(rec.map_id, rec.group_id, rec.floor_id);
                }
            }
        }

        // 5. Active Route legs drawn beneath waypoints & pins (matches M-Map section colors).
        if !scene.route_legs.is_empty() {
            let stroke_done = Stroke {
                width: 1.7,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                dash: tiny_skia::StrokeDash::new(vec![5.0, 4.5], 0.0),
                ..Stroke::default()
            };
            let mut p_done = Paint {
                anti_alias: true,
                ..Default::default()
            };
            p_done.set_color_rgba8(148, 163, 184, 125);

            for leg in scene
                .route_legs
                .iter()
                .filter(|l| !l.emphasized)
                .chain(scene.route_legs.iter().filter(|l| l.emphasized))
            {
                let (x0, y0) = world_to_radar_px(leg.from_lng, leg.from_lat, rx, ry, zoom, cx, cy);
                let (x1, y1) = world_to_radar_px(leg.to_lng, leg.to_lat, rx, ry, zoom, cx, cy);
                if (x0 < -24.0 && x1 < -24.0)
                    || (x0 > wf + 24.0 && x1 > wf + 24.0)
                    || (y0 < -24.0 && y1 < -24.0)
                    || (y0 > hf + 24.0 && y1 > hf + 24.0)
                {
                    continue;
                }
                let mut pb = PathBuilder::new();
                pb.move_to(x0, y0);
                pb.line_to(x1, y1);
                if let Some(path) = pb.finish() {
                    if leg.done {
                        pm.stroke_path(
                            &path,
                            &p_done,
                            &stroke_done,
                            Transform::identity(),
                            self.clip_mask.as_ref(),
                        );
                    } else {
                        let [r, g, b, base_a] = leg.color;
                        let alpha = if leg.emphasized {
                            base_a.max(210)
                        } else {
                            base_a.min(130)
                        };
                        let w_line = if leg.emphasized { 2.8 } else { 1.8 };
                        let mut p_casing = Paint {
                            anti_alias: true,
                            ..Default::default()
                        };
                        p_casing.set_color_rgba8(2, 6, 23, alpha / 2);
                        let st_casing = Stroke {
                            width: w_line + 2.6,
                            line_cap: LineCap::Round,
                            line_join: LineJoin::Round,
                            ..Stroke::default()
                        };
                        pm.stroke_path(
                            &path,
                            &p_casing,
                            &st_casing,
                            Transform::identity(),
                            self.clip_mask.as_ref(),
                        );

                        let mut p_line = Paint {
                            anti_alias: true,
                            ..Default::default()
                        };
                        p_line.set_color_rgba8(r, g, b, alpha);
                        let st_line = Stroke {
                            width: w_line,
                            line_cap: LineCap::Round,
                            line_join: LineJoin::Round,
                            ..Stroke::default()
                        };
                        pm.stroke_path(
                            &path,
                            &p_line,
                            &st_line,
                            Transform::identity(),
                            self.clip_mask.as_ref(),
                        );

                        // Subtle route node joints matching M-Map so intermediate bends remain crisp.
                        if leg.emphasized {
                            for (nx, ny) in [(x0, y0), (x1, y1)] {
                                if nx < -8.0 || nx > wf + 8.0 || ny < -8.0 || ny > hf + 8.0 {
                                    continue;
                                }
                                if let Some(outer) = PathBuilder::from_circle(nx, ny, 3.6) {
                                    pm.fill_path(
                                        &outer,
                                        &p_casing,
                                        FillRule::Winding,
                                        Transform::identity(),
                                        self.clip_mask.as_ref(),
                                    );
                                }
                                if let Some(inner) = PathBuilder::from_circle(nx, ny, 2.2) {
                                    pm.fill_path(
                                        &inner,
                                        &p_line,
                                        FillRule::Winding,
                                        Transform::identity(),
                                        self.clip_mask.as_ref(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // 6. Primary Teleports / Statues / Domains rendered with official HoYoLAB assets (subpixel, floor-aware).
        //    Highlights any waypoint targeted by the active Route (stop or suggested anchor teleport).
        let cur_icon_gen = self.assets.icon_generation();
        self.ensure_ui_sprites_queued(cur_icon_gen);
        let ui_sprites = {
            let b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
            b.ui_sprites.clone()
        };
        let mut target_rendered = false;
        let mut anchor_rendered = false;
        if let Some(mdata) = maps_db.map(scene.map_id) {
            let mut deferred_wps: Vec<(&gh_core::multimap::WaypointEntry, f32, f32, bool, [u8; 4])> =
                Vec::new();
            for wp in &mdata.waypoints {
                if !wp.is_primary || wp.label_id <= 0 {
                    continue;
                }
                let (sx, sy) = world_to_radar_px(wp.x_pos, wp.y_pos, rx, ry, zoom, cx, cy);
                if sx < -28.0 || sx > wf + 28.0 || sy < -28.0 || sy > hf + 40.0 {
                    continue;
                }
                let matches_hint = |h: &RadarTargetHint| {
                    h.id == wp.id.to_string()
                        || ((wp.x_pos - h.lng).hypot(wp.y_pos - h.lat) <= 14.0
                            && (h.is_route_stop || h.is_locked))
                };
                let hit_target = scene.target.as_ref().is_some_and(matches_hint);
                let hit_anchor = scene.route_anchor.as_ref().is_some_and(matches_hint);
                if hit_target {
                    target_rendered = true;
                }
                if hit_anchor {
                    anchor_rendered = true;
                }
                if hit_target || hit_anchor {
                    let accent = scene
                        .target
                        .as_ref()
                        .filter(|_| hit_target)
                        .and_then(|t| t.accent)
                        .or_else(|| scene.route_anchor.as_ref().and_then(|a| a.accent))
                        .unwrap_or(DEFAULT_RADAR_ROUTE_COLOR);
                    deferred_wps.push((wp, sx, sy, true, accent));
                    continue;
                }

                let wp_fid = wp.point_group.as_ref().and_then(|pg| pg.floor_id).filter(|&f| f != 0);
                let wp_is_ug = wp.z_level != 0 || wp.icon_sign != 0 || wp_fid.is_some();
                let wp_same_floor = if !scene.is_underground && !wp_is_ug {
                    true
                } else if scene.is_underground != wp_is_ug {
                    false
                } else {
                    match (scene.floor_id, wp_fid) {
                        (Some(pf), Some(wfid)) => pf == wfid,
                        _ => true,
                    }
                };
                let wp_icon = {
                    let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                    b.icons.get(&(scene.map_id, wp.label_id))
                };
                if wp_icon.is_none() {
                    self.queue_icon_decode(scene.map_id, wp.label_id, cur_icon_gen);
                }
                let eff_sign = if wp.icon_sign != 0 {
                    wp.icon_sign
                } else if wp_is_ug && wp.z_level == 0 {
                    1
                } else {
                    0
                };
                let opacity = if wp_same_floor { 0.95 } else { 0.40 };
                draw_hoyolab_marker(
                    &mut pm,
                    sx,
                    sy,
                    RADAR_WAYPOINT_SCALE,
                    wp_icon.as_deref(),
                    &ui_sprites,
                    false,
                    false,
                    eff_sign,
                    wp.z_level,
                    opacity,
                    self.clip_mask.as_ref(),
                );
            }
            for (wp, sx, sy, _, accent) in deferred_wps {
                let wp_fid = wp.point_group.as_ref().and_then(|pg| pg.floor_id).filter(|&f| f != 0);
                let wp_is_ug = wp.z_level != 0 || wp.icon_sign != 0 || wp_fid.is_some();
                let wp_icon = {
                    let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                    b.icons.get(&(scene.map_id, wp.label_id))
                };
                if wp_icon.is_none() {
                    self.queue_icon_decode(scene.map_id, wp.label_id, cur_icon_gen);
                }
                let eff_sign = if wp.icon_sign != 0 {
                    wp.icon_sign
                } else if wp_is_ug && wp.z_level == 0 {
                    1
                } else {
                    0
                };
                let hi_scale = RADAR_WAYPOINT_SCALE * 1.24;
                draw_target_beacon_ring(
                    &mut pm,
                    sx,
                    sy - 19.85 * hi_scale,
                    16.5,
                    accent,
                    self.clip_mask.as_ref(),
                );
                draw_hoyolab_marker(
                    &mut pm,
                    sx,
                    sy,
                    hi_scale,
                    wp_icon.as_deref(),
                    &ui_sprites,
                    true,
                    true,
                    eff_sign,
                    wp.z_level,
                    1.0,
                    self.clip_mask.as_ref(),
                );
            }
        }

        // 7. Pins from `/lan_pins` + active Route stops (Phương án A), filtered live by `excluded_ids`.
        let player_fid_str = scene.floor_id.map(|f| f.to_string());
        let target_id = scene.target.as_ref().map(|t| t.id.as_str());
        let target_accent = scene
            .target
            .as_ref()
            .and_then(|t| t.accent)
            .unwrap_or(DEFAULT_RADAR_ROUTE_COLOR);
        let mut visible_pins: Vec<(&RadarPin, f32, f32, bool, bool)> = Vec::new();

        for pin in scene.pins.iter() {
            if pin.marked || scene.excluded_ids.contains(&pin.id) {
                continue;
            }
            let (sx, sy) = world_to_radar_px(pin.lng, pin.lat, rx, ry, zoom, cx, cy);
            if sx < -26.0 || sx > wf + 26.0 || sy < -26.0 || sy > hf + 42.0 {
                continue;
            }
            let same_floor = is_pin_same_floor(pin, scene.is_underground, player_fid_str.as_deref());
            let is_target = target_id == Some(pin.id.as_str());
            if is_target {
                target_rendered = true;
            }
            visible_pins.push((pin, sx, sy, same_floor, is_target));
        }
        // Back-to-front ordering: off-floor first, same-floor middle, active target last.
        visible_pins.sort_by_key(|(_, _, _, same_floor, is_target)| (*is_target as u8) * 2 + (*same_floor as u8));

        for (pin, sx, sy, same_floor, is_target) in visible_pins {
            let is_locked = is_target
                && scene
                    .target
                    .as_ref()
                    .is_some_and(|t| t.is_locked || t.is_route_stop);
            let icon_pm = if pin.label_id > 0 {
                let cached = {
                    let mut b = self.bank.lock().unwrap_or_else(|e| e.into_inner());
                    b.icons.get(&(scene.map_id, pin.label_id))
                };
                if cached.is_none() {
                    self.queue_icon_decode(scene.map_id, pin.label_id, cur_icon_gen);
                }
                cached
            } else {
                None
            };
            let eff_sign = if pin.icon_sign != 0 {
                pin.icon_sign
            } else if pin.is_underground && pin.z_level == 0 {
                1
            } else {
                0
            };
            let opacity = if same_floor || is_target { 1.0 } else { 0.42 };
            let scale = if is_target {
                RADAR_PIN_SCALE * 1.18
            } else {
                RADAR_PIN_SCALE
            };
            if is_target && scene.target.as_ref().is_some_and(|t| t.is_route_stop) {
                draw_target_beacon_ring(
                    &mut pm,
                    sx,
                    sy - 19.85 * scale,
                    17.5,
                    target_accent,
                    self.clip_mask.as_ref(),
                );
            }
            draw_hoyolab_marker(
                &mut pm,
                sx,
                sy,
                scale,
                icon_pm.as_deref(),
                &ui_sprites,
                is_target,
                is_locked,
                eff_sign,
                pin.z_level,
                opacity,
                self.clip_mask.as_ref(),
            );
            // Custom route stops without a HoYoLAB label_id get a crisp accent center dot inside the pin head.
            if pin.label_id <= 0
                && icon_pm.is_none()
                && let Some(dot) = PathBuilder::from_circle(sx, sy - 19.85 * scale, 6.2 * scale)
            {
                let mut dp = Paint {
                    anti_alias: true,
                    ..Default::default()
                };
                let [r, g, b, _] = target_accent;
                dp.set_color_rgba8(r, g, b, (240.0 * opacity).round() as u8);
                pm.fill_path(&dot, &dp, FillRule::Winding, Transform::identity(), self.clip_mask.as_ref());
            }
        }

        // 7b. On-screen glowing beacon for non-pin route targets/anchors (e.g. walking waypoints or custom teleports).
        let draw_hint_beacon = |pm: &mut Pixmap, hint: &RadarTargetHint, radius: f32| {
            let (tx, ty) = world_to_radar_px(hint.lng, hint.lat, rx, ry, zoom, cx, cy);
            if tx >= 6.0 && tx <= wf - 6.0 && ty >= 6.0 && ty <= hf - 6.0 {
                let accent = hint.accent.unwrap_or(DEFAULT_RADAR_ROUTE_COLOR);
                draw_target_beacon_ring(pm, tx, ty, radius, accent, self.clip_mask.as_ref());
                if let Some(core) = PathBuilder::from_circle(tx, ty, 3.8) {
                    let mut cp = Paint {
                        anti_alias: true,
                        ..Default::default()
                    };
                    cp.set_color_rgba8(255, 255, 255, 245);
                    pm.fill_path(&core, &cp, FillRule::Winding, Transform::identity(), self.clip_mask.as_ref());
                }
            }
        };
        if !anchor_rendered
            && let Some(ref anc) = scene.route_anchor
        {
            draw_hint_beacon(&mut pm, anc, 14.5);
        }
        if !target_rendered
            && let Some(ref tgt) = scene.target
            && tgt.is_route_stop
        {
            draw_hint_beacon(&mut pm, tgt, 15.5);
        }

        // Off-screen locked/active target edge pointer tucked neatly inside card border.
        if let Some(ref tgt) = scene.target {
            let (tx, ty) = world_to_radar_px(tgt.lng, tgt.lat, rx, ry, zoom, cx, cy);
            let margin = 16.0;
            if tx < margin || tx > wf - margin || ty < margin || ty > hf - margin {
                draw_offscreen_target_edge_arrow(
                    &mut pm,
                    cx,
                    cy,
                    tx,
                    ty,
                    wf,
                    hf,
                    tgt.is_locked || tgt.is_route_stop,
                    self.clip_mask.as_ref(),
                );
            }
        }

        // 8. Center Player Arrow + Camera FOV Cone (60 Hz, matches Genshin minimap).
        draw_player_fov_and_arrow(&mut pm, cx, cy, cam_deg, body_deg, self.clip_mask.as_ref());

        // 9. Subtle inner edge vignette so map dissolves softly into rounded corners.
        draw_inner_vignette(&mut pm, pad, wf, hf, radius, self.clip_mask.as_ref());

        // 10. Ultra-thin Teyvat Celestial Hairline border + 4 delicate corner accents.
        draw_celestial_frame(&mut pm, pad, wf, hf, radius, edit_mode);

        // 11. Minimalist contextual micro-badges (`N` pip, underground floor pill, Edit Mode overlay).
        self.draw_micro_badges(&mut pm, wf, hf, pad, scene, zoom, edit_mode);

        self.pixmap = Some(pm);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_micro_badges(
        &mut self,
        pm: &mut Pixmap,
        wf: f32,
        hf: f32,
        pad: f32,
        scene: &RadarScene,
        zoom: f64,
        edit_mode: bool,
    ) {
        // Subtle `N` compass pip at top-center (North-Up orientation cue).
        let n_style = TextStyle::new(10.5, FontWeightRole::Bold, [236, 229, 216, 175])
            .with_line_height(12.0);
        let n_fx = TextEffect::Shadow {
            offset_x: 1,
            offset_y: 1,
            color: [6, 9, 16, 210],
        };
        let _ = self.fonts.draw_text(
            &mut pm.as_mut(),
            "N",
            wf * 0.5 - 10.0,
            pad + 3.0,
            Some(20.0),
            TextAlign::Center,
            Wrap::None,
            Ellipsize::None,
            n_style,
            n_fx,
        );

        // Underground floor pill (top-left) ONLY when underground.
        if scene.is_underground && !scene.floor_short.is_empty() {
            let pill_w = (scene.floor_short.chars().count() as f32 * 7.5 + 14.0).clamp(28.0, 66.0);
            let pill_h = 18.0;
            let px = pad + 8.0;
            let py = pad + 7.0;
            if let Some(bg) = build_rounded_rect_path(px, py, pill_w, pill_h, 6.0) {
                let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
                p.set_color_rgba8(14, 18, 28, 195);
                pm.fill_path(&bg, &p, FillRule::Winding, Transform::identity(), None);
                let mut rim = Paint {
                    anti_alias: true,
                    ..Default::default()
                };
                rim.set_color_rgba8(211, 188, 142, 130);
                let st = Stroke {
                    width: 1.0,
                    ..Stroke::default()
                };
                pm.stroke_path(&bg, &rim, &st, Transform::identity(), None);
            }
            let fl_style = TextStyle::new(10.5, FontWeightRole::Bold, [211, 188, 142, 245])
                .with_line_height(14.0);
            let fl_line = self
                .fonts
                .truncate_line(&scene.floor_short, fl_style, (pill_w - 4.0).max(12.0));
            let _ = self.fonts.draw_text(
                &mut pm.as_mut(),
                &fl_line,
                px,
                py + 2.0,
                Some(pill_w),
                TextAlign::Center,
                Wrap::None,
                Ellipsize::None,
                fl_style,
                TextEffect::None,
            );
        }

        // Edit Mode size/zoom badge + 1-line hint.
        if edit_mode {
            let zoom_mult = 2f64.powf(zoom - DEFAULT_RADAR_ZOOM);
            let dim_text = format!(
                "{} × {}  ·  {:.2}×",
                wf.round() as i32,
                hf.round() as i32,
                zoom_mult
            );
            let pill_w = 148.0f32.min(wf - 20.0);
            let px = (wf - pill_w) * 0.5;
            let py = pad + 20.0;
            if let Some(bg) = build_rounded_rect_path(px, py, pill_w, 22.0, 7.0) {
                let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
                p.set_color_rgba8(14, 18, 28, 215);
                pm.fill_path(&bg, &p, FillRule::Winding, Transform::identity(), None);
            }
            let dim_style = TextStyle::new(11.5, FontWeightRole::Bold, [248, 250, 252, 245])
                .with_line_height(15.0);
            let dim_line = self
                .fonts
                .truncate_line(&dim_text, dim_style, (pill_w - 6.0).max(20.0));
            let _ = self.fonts.draw_text(
                &mut pm.as_mut(),
                &dim_line,
                px,
                py + 3.0,
                Some(pill_w),
                TextAlign::Center,
                Wrap::None,
                Ellipsize::None,
                dim_style,
                TextEffect::None,
            );

            if !scene.edit_hint.is_empty() && hf >= 140.0 {
                let hint_w = (wf - 16.0).max(40.0);
                let hy = hf - 26.0;
                if let Some(bg) = build_rounded_rect_path(8.0, hy, hint_w, 19.0, 6.0) {
                    let mut p = Paint {
                    anti_alias: true,
                    ..Default::default()
                };
                    p.set_color_rgba8(14, 18, 28, 210);
                    pm.fill_path(&bg, &p, FillRule::Winding, Transform::identity(), None);
                }
                let hint_style =
                    TextStyle::new(10.0, FontWeightRole::Regular, [236, 229, 216, 235])
                        .with_line_height(13.0);
                let hint_line = self.fonts.truncate_line(
                    &scene.edit_hint,
                    hint_style,
                    (hint_w - 8.0).max(24.0),
                );
                let _ = self.fonts.draw_text(
                    &mut pm.as_mut(),
                    &hint_line,
                    12.0,
                    hy + 3.0,
                    Some(hint_w - 8.0),
                    TextAlign::Center,
                    Wrap::None,
                    Ellipsize::None,
                    hint_style,
                    TextEffect::None,
                );
            }
        }
    }

    fn persist_layout(&self, rect: Option<RectI32>) {
        let (rel, zoom, opacity) = {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if let (Some(w_rect), Some(sc)) = (rect, g.scene.as_ref())
                && let Some(new_rel) = radar_rect_to_relative(w_rect, sc.game_rect)
            {
                g.rel_rect = Some(new_rel);
            }
            (g.rel_rect, g.zoom, g.opacity)
        };
        if let Some(ref cb) = self.on_layout_saved {
            cb(rel, zoom, opacity);
        }
    }
}

impl LayeredHandler for RadarUiState {
    fn on_app_message(
        &mut self,
        ctx: &mut LayeredWindowCtx<'_>,
        msg: u32,
        wparam: usize,
        _lparam: isize,
    ) -> Option<isize> {
        match msg {
            WM_APP_RADAR_UPDATE | WM_APP_RADAR_ASSET_READY => {
                self.step_and_render(ctx, true);
                Some(0)
            }
            WM_APP_RADAR_HIDE => {
                self.hide_clean(ctx);
                Some(0)
            }
            WM_APP_RADAR_EDIT => {
                let want = wparam != 0;
                {
                    let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    g.edit_mode = want;
                }
                ctx.set_edit_mode(want);
                if !want {
                    self.live_interaction_rect = None;
                }
                self.step_and_render(ctx, true);
                Some(0)
            }
            _ => None,
        }
    }

    fn on_timer(&mut self, ctx: &mut LayeredWindowCtx<'_>, timer_id: usize) {
        if timer_id == TIMER_ID_RADAR_ANIM {
            self.step_and_render(ctx, false);
        } else if timer_id == TIMER_ID_RADAR_LEASE {
            let (edit_mode, expired) = {
                let g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                (
                    g.edit_mode,
                    g.last_scene_at
                        .is_none_or(|t0| t0.elapsed() > Duration::from_millis(RADAR_LEASE_MS)),
                )
            };
            if !edit_mode && expired {
                self.hide_clean(ctx);
            }
        }
    }

    fn on_moving(&mut self, ctx: &mut LayeredWindowCtx<'_>, window_rect: RectI32) {
        self.live_interaction_rect = Some(window_rect);
        self.last_presented_rect = Some(window_rect);
        let _ = ctx.present_position(window_rect.x, window_rect.y, self.last_effective_alpha);
    }

    fn on_sizing(&mut self, ctx: &mut LayeredWindowCtx<'_>, window_rect: RectI32) {
        self.live_interaction_rect = Some(window_rect);
        self.step_and_render(ctx, true);
    }

    fn on_drag_end(&mut self, ctx: &mut LayeredWindowCtx<'_>, window_rect: RectI32) {
        self.live_interaction_rect = None;
        if window_rect.width > 0 && window_rect.height > 0 {
            self.persist_layout(Some(window_rect));
        }
        self.step_and_render(ctx, true);
    }

    fn on_wheel(&mut self, ctx: &mut LayeredWindowCtx<'_>, delta: i16, ctrl_held: bool) {
        if delta == 0 {
            return;
        }
        let step_sign = if delta > 0 { 1.0 } else { -1.0 };
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if ctrl_held {
                g.opacity = ((g.opacity + step_sign * 0.04)
                    .clamp(RADAR_OPACITY_MIN, RADAR_OPACITY_MAX)
                    * 100.0)
                    .round()
                    / 100.0;
            } else {
                g.zoom = ((g.zoom + step_sign * 0.12).clamp(RADAR_ZOOM_MIN, RADAR_ZOOM_MAX) * 100.0)
                    .round()
                    / 100.0;
            }
        }
        self.persist_layout(None);
        self.step_and_render(ctx, true);
    }

    fn on_destroy(&mut self, ctx: &mut LayeredWindowCtx<'_>) {
        self.ensure_anim_timer(ctx, false);
        self.ensure_lease_timer(ctx, false);
        self.pixmap = None;
        self.clip_mask = None;
        self.bank.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

// ---------------------------------------------------------------------------
// Public Controller Handle (Created ONLY when Radar is ON, Dropped when OFF!)
// ---------------------------------------------------------------------------

pub struct RadarMapController {
    host: Arc<LayeredWindowHost>,
    shared: Arc<Mutex<RadarShared>>,
    bank: Arc<Mutex<DecodedAssetBank>>,
    decode_tx: Mutex<Option<mpsc::SyncSender<DecodeReq>>>,
    decode_thread: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl RadarMapController {
    pub fn new(
        assets: Arc<dyn RadarAssetSource>,
        rel_rect: Option<[f64; 4]>,
        zoom: f64,
        opacity: f64,
        capture_visible: bool,
        on_layout_saved: Option<RadarLayoutCallback>,
    ) -> Self {
        Self::new_with_offset(
            assets,
            rel_rect,
            zoom,
            opacity,
            capture_visible,
            on_layout_saved,
            (0, 0),
        )
    }

    pub fn new_with_offset(
        assets: Arc<dyn RadarAssetSource>,
        rel_rect: Option<[f64; 4]>,
        zoom: f64,
        opacity: f64,
        capture_visible: bool,
        on_layout_saved: Option<RadarLayoutCallback>,
        screen_offset: (i32, i32),
    ) -> Self {
        let host = Arc::new(LayeredWindowHost::new(LayeredWindowConfig {
            class_prefix: "GenshinHelperRadarMap",
            window_title: "GenshinHelperRadarMap",
            thread_name: "GH_RadarMap",
            screen_offset,
            enable_wda: true,
            initial_capture_visible: false,
            wda_refresh_ms: crate::layered::WDA_REFRESH_MS,
            resizable: true,
            clamp_to_monitor: true,
            min_size: RADAR_MIN_SIZE,
            max_size: RADAR_MAX_SIZE,
            resize_border_px: 10,
        }));

        let shared = Arc::new(Mutex::new(RadarShared {
            scene: None,
            last_scene_at: None,
            edit_mode: false,
            rel_rect,
            zoom: zoom.clamp(RADAR_ZOOM_MIN, RADAR_ZOOM_MAX),
            opacity: opacity.clamp(RADAR_OPACITY_MIN, RADAR_OPACITY_MAX),
            capture_visible,
            last_tile_gen: assets.tile_generation(),
            last_icon_gen: assets.icon_generation(),
        }));
        let bank = Arc::new(Mutex::new(DecodedAssetBank::new()));
        let (decode_tx, decode_rx) = mpsc::sync_channel::<DecodeReq>(96);

        let assets_dec = Arc::clone(&assets);
        let bank_dec = Arc::clone(&bank);
        let host_dec = Arc::clone(&host);
        let decode_handle = thread::Builder::new()
            .name("GH_RadarDecode".to_owned())
            .spawn(move || {
                for req in decode_rx {
                    match req {
                        DecodeReq::Shutdown => break,
                        DecodeReq::Tile(mid, z, tx, ty) => {
                            let decoded = assets_dec
                                .get_color_tile_bytes(mid, z, tx, ty)
                                .and_then(|b| decode_image_bytes_to_pixmap(&b));
                            let mut b = bank_dec.lock().unwrap_or_else(|e| e.into_inner());
                            b.pending_tiles.remove(&(mid, z, tx, ty));
                            if let Some(pm) = decoded {
                                b.tiles.insert((mid, z, tx, ty), pm);
                                drop(b);
                                let _ = host_dec.post_message(WM_APP_RADAR_ASSET_READY, 0, 0);
                            }
                        }
                        DecodeReq::Icon(mid, lid, req_gen) => {
                            let decoded = assets_dec
                                .get_icon_bytes_or_request(mid, lid)
                                .and_then(|b| decode_circular_category_icon(&b));
                            let mut b = bank_dec.lock().unwrap_or_else(|e| e.into_inner());
                            b.pending_icons.remove(&(mid, lid));
                            if let Some(pm) = decoded {
                                b.icon_miss_gen.remove(&(mid, lid));
                                b.icons.insert((mid, lid), pm);
                                drop(b);
                                let _ = host_dec.post_message(WM_APP_RADAR_ASSET_READY, 0, 0);
                            } else {
                                b.icon_miss_gen.insert((mid, lid), req_gen);
                            }
                        }
                        DecodeReq::UiSprite(kind, req_gen) => {
                            let decoded = assets_dec
                                .get_marker_ui_asset_or_request(kind.key())
                                .and_then(|b| decode_image_bytes_to_pixmap(&b));
                            let mut b = bank_dec.lock().unwrap_or_else(|e| e.into_inner());
                            b.pending_ui.remove(&kind);
                            if let Some(pm) = decoded {
                                b.ui_miss_gen.remove(&kind);
                                b.ui_sprites.insert(kind, pm);
                                drop(b);
                                let _ = host_dec.post_message(WM_APP_RADAR_ASSET_READY, 0, 0);
                            } else {
                                b.ui_miss_gen.insert(kind, req_gen);
                            }
                        }
                        DecodeReq::Ug(mid, gid, fid) => {
                            let decoded = assets_dec
                                .get_ug_overlay_bytes(mid, gid, fid)
                                .and_then(|b| decode_image_bytes_to_pixmap(&b));
                            let mut b = bank_dec.lock().unwrap_or_else(|e| e.into_inner());
                            b.pending_ug.remove(&(mid, gid, fid));
                            if let Some(pm) = decoded {
                                b.ug.insert((mid, gid, fid), pm);
                                drop(b);
                                let _ = host_dec.post_message(WM_APP_RADAR_ASSET_READY, 0, 0);
                            }
                        }
                    }
                }
            })
            .ok();

        let ui_state = RadarUiState {
            shared: Arc::clone(&shared),
            assets,
            bank: Arc::clone(&bank),
            decode_tx: decode_tx.clone(),
            on_layout_saved,
            fonts: HudFonts::new(),
            pixmap: None,
            clip_mask: None,
            clip_mask_size: (0, 0),
            smooth_wx: None,
            smooth_wy: None,
            smooth_vx: 0.0,
            smooth_vy: 0.0,
            smooth_body_deg: None,
            smooth_cam_deg: None,
            last_map_id: None,
            last_anim_ts: None,
            anim_timer_active: false,
            lease_timer_active: false,
            hover_fade: HoverFadeController::default(),
            last_presented_rect: None,
            last_forced_wda_exclude: None,
            last_effective_alpha: 240,
            live_interaction_rect: None,
            last_prefetch_ts: None,
            sync_decode: false,
            sync_decoded_any: AtomicBool::new(false),
        };
        let _ = host.ensure_started(move || ui_state);

        Self {
            host,
            shared,
            bank,
            decode_tx: Mutex::new(Some(decode_tx)),
            decode_thread: Mutex::new(decode_handle),
            closed: AtomicBool::new(false),
        }
    }

    pub fn update(&self, scene: RadarScene) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.capture_visible = scene.capture_visible;
            g.scene = Some(scene);
            g.last_scene_at = Some(Instant::now());
        }
        let _ = self.host.post_message(WM_APP_RADAR_UPDATE, 0, 0);
    }

    pub fn hide(&self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.last_scene_at = None;
        }
        if self.host.is_thread_running() {
            let _ = self.host.post_message(WM_APP_RADAR_HIDE, 0, 0);
        }
    }

    pub fn set_edit_mode(&self, enabled: bool) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.edit_mode = enabled;
        }
        if self.host.is_thread_running() {
            let _ = self
                .host
                .post_message(WM_APP_RADAR_EDIT, usize::from(enabled), 0);
        }
    }

    pub fn is_edit_mode(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .edit_mode
    }

    pub fn set_layout_params(&self, rel_rect: Option<[f64; 4]>, zoom: f64, opacity: f64) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.rel_rect = rel_rect;
            g.zoom = zoom.clamp(RADAR_ZOOM_MIN, RADAR_ZOOM_MAX);
            g.opacity = opacity.clamp(RADAR_OPACITY_MIN, RADAR_OPACITY_MAX);
        }
        if self.host.is_thread_running() {
            let _ = self.host.post_message(WM_APP_RADAR_UPDATE, 0, 0);
        }
    }

    pub fn set_capture_visible(&self, visible: bool) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.capture_visible = visible;
        }
        if self.host.is_thread_running() {
            let _ = self.host.post_message(WM_APP_RADAR_UPDATE, 0, 0);
        }
    }

    pub fn is_visible(&self) -> bool {
        self.host.is_visible()
    }

    pub fn is_running(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.host.is_thread_running()
    }

    pub fn raw_hwnd(&self) -> Option<isize> {
        self.host.raw_hwnd()
    }

    /// Tears down the Win32 layered window, UI thread, decoder thread, and all RGBA LRU caches
    /// cleanly so turning Radar OFF leaves 0 threads, 0 HWND, and 0 MB cached RAM behind.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(tx) = self
            .decode_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.try_send(DecodeReq::Shutdown);
        }
        self.host.close();
        if let Some(handle) = self
            .decode_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        self.bank.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        g.scene = None;
        g.last_scene_at = None;
    }
}

impl Drop for RadarMapController {
    fn drop(&mut self) {
        self.close();
    }
}

/// Pure, zero-HWND, synchronous Radar renderer for headless QA visual verification (`gh-sim`).
///
/// Reuses the exact production `RadarUiState::render_frame_into_pixmap` drawing pipeline while
/// decoding offline tiles/overlays/icons inline from [`RadarAssetSource`] without spawning any
/// Win32 window or background thread.
pub struct HeadlessRadarRenderer {
    ui: RadarUiState,
}

impl HeadlessRadarRenderer {
    pub fn new(assets: Arc<dyn RadarAssetSource>) -> Self {
        let shared = Arc::new(Mutex::new(RadarShared {
            scene: None,
            edit_mode: false,
            rel_rect: None,
            zoom: DEFAULT_RADAR_ZOOM,
            opacity: 0.94,
            capture_visible: false,
            last_scene_at: None,
            last_tile_gen: 0,
            last_icon_gen: 0,
        }));
        let bank = Arc::new(Mutex::new(DecodedAssetBank::new()));
        let (decode_tx, _decode_rx) = mpsc::sync_channel::<DecodeReq>(1);
        let ui = RadarUiState {
            shared,
            assets,
            bank,
            decode_tx,
            on_layout_saved: None,
            fonts: HudFonts::new(),
            pixmap: None,
            clip_mask: None,
            clip_mask_size: (0, 0),
            smooth_wx: None,
            smooth_wy: None,
            smooth_vx: 0.0,
            smooth_vy: 0.0,
            smooth_body_deg: None,
            smooth_cam_deg: None,
            last_map_id: None,
            last_anim_ts: None,
            anim_timer_active: false,
            lease_timer_active: false,
            hover_fade: HoverFadeController::default(),
            last_presented_rect: None,
            last_forced_wda_exclude: None,
            last_effective_alpha: 240,
            live_interaction_rect: None,
            last_prefetch_ts: None,
            sync_decode: true,
            sync_decoded_any: AtomicBool::new(false),
        };
        Self { ui }
    }

    /// Renders `scene` synchronously into a `w × h` premultiplied RGBA [`Pixmap`].
    /// Any missing tiles/overlays/icons available from [`RadarAssetSource`] are decoded inline
    /// before the returned pixmap is finalized so the first frame is already populated.
    pub fn render_scene(
        &mut self,
        scene: &RadarScene,
        w: u32,
        h: u32,
        zoom: f64,
        edit_mode: bool,
    ) -> Option<&Pixmap> {
        let zoom_clamped = zoom.clamp(RADAR_ZOOM_MIN, RADAR_ZOOM_MAX);
        let body_deg = scene.body_heading.or(scene.camera_heading).unwrap_or(0.0);
        let cam_deg = scene.camera_heading.or(scene.body_heading).unwrap_or(body_deg);
        self.ui.sync_decoded_any.store(false, Ordering::Relaxed);
        self.ui.render_frame_into_pixmap(
            w,
            h,
            scene,
            scene.world_x,
            scene.world_y,
            body_deg,
            cam_deg,
            zoom_clamped,
            edit_mode,
        );
        if self.ui.sync_decoded_any.swap(false, Ordering::Relaxed) {
            self.ui.render_frame_into_pixmap(
                w,
                h,
                scene,
                scene.world_x,
                scene.world_y,
                body_deg,
                cam_deg,
                zoom_clamped,
                edit_mode,
            );
        }
        self.ui.pixmap.as_ref()
    }

    /// Convenience wrapper returning `(width, height, premultiplied_rgba_bytes)` so callers
    /// (such as `gh-sim`) do not need a direct `tiny-skia` dependency.
    pub fn render_scene_rgba(
        &mut self,
        scene: &RadarScene,
        w: u32,
        h: u32,
        zoom: f64,
        edit_mode: bool,
    ) -> Option<(u32, u32, &[u8])> {
        let pm = self.render_scene(scene, w, h, zoom, edit_mode)?;
        Some((pm.width(), pm.height(), pm.data()))
    }
}

// ---------------------------------------------------------------------------
// Pure Drawing & Matching Helpers
// ---------------------------------------------------------------------------

fn is_pin_same_floor(pin: &RadarPin, player_ug: bool, player_fid: Option<&str>) -> bool {
    if !player_ug && !pin.is_underground {
        return true;
    }
    if player_ug != pin.is_underground {
        return false;
    }
    match (player_fid, pin.floor_id.as_deref()) {
        (Some(pf), Some(tf)) if !pf.is_empty() && !tf.is_empty() => pf == tf,
        _ => true,
    }
}

fn resolve_active_floor_overlay(
    db: &MultimapDb,
    scene: &RadarScene,
    rx: f64,
    ry: f64,
) -> Option<FloorOverlayRecord> {
    let records = db.floor_overlays_for_map(scene.map_id);
    if records.is_empty() {
        return None;
    }
    if let (Some(gid), Some(fid)) = (scene.group_id, scene.floor_id)
        && let Some(rec) = records
            .iter()
            .find(|r| r.group_id == gid && r.floor_id == fid)
    {
        return Some(rec.clone());
    }
    if let Some(fid) = scene.floor_id
        && let Some(rec) = records.iter().find(|r| r.floor_id == fid)
    {
        return Some(rec.clone());
    }
    // Fallback: closest overlapping floor containing `(rx, ry)`.
    records.into_iter().find(|r| {
        let (min_lng, max_lng, min_lat, max_lat) = r.overlay.bounds();
        rx >= min_lng - 25.0 && rx <= max_lng + 25.0 && ry >= min_lat - 25.0 && ry <= max_lat + 25.0
    })
}

fn build_rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let rr = r.clamp(0.0, (w * 0.5).min(h * 0.5));
    let mut pb = PathBuilder::new();
    if rr < 0.5 {
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        return pb.finish();
    }
    const K: f32 = 0.552_284_8;
    let c = rr * K;
    pb.move_to(x + rr, y);
    pb.line_to(x + w - rr, y);
    pb.cubic_to(x + w - rr + c, y, x + w, y + rr - c, x + w, y + rr);
    pb.line_to(x + w, y + h - rr);
    pb.cubic_to(
        x + w,
        y + h - rr + c,
        x + w - rr + c,
        y + h,
        x + w - rr,
        y + h,
    );
    pb.line_to(x + rr, y + h);
    pb.cubic_to(x + rr - c, y + h, x, y + h - rr + c, x, y + h - rr);
    pb.line_to(x, y + rr);
    pb.cubic_to(x, y + rr - c, x + rr - c, y, x + rr, y);
    pb.close();
    pb.finish()
}

#[inline]
fn draw_scaled_sprite(
    pm: &mut Pixmap,
    sprite: &Pixmap,
    dst: [f32; 4],
    opacity: f32,
    mask: Option<&Mask>,
) {
    let [x, y, w, h] = dst;
    let sw = sprite.width().max(1) as f32;
    let sh = sprite.height().max(1) as f32;
    let tf = Transform::from_scale(w / sw, h / sh).post_translate(x, y);
    let paint = PixmapPaint {
        opacity: opacity.clamp(0.0, 1.0),
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    pm.draw_pixmap(0, 0, sprite.as_ref(), &paint, tf, mask);
}

/// Renders a map marker (collectible pin or waypoint) using HoYoLAB Interactive Map's exact
/// official `ciLayer._drawImage` assets, bottom-tip anchor `(sx, sy)`, and layer stack:
/// `SelectedFrame` -> `BgNormal/BgSelected` -> circular category icon -> `UndergroundMark` /
/// `WaterholeMark` (`icon_sign`) -> `ZLevelUpper/Middle/Lower` (`z_level`).
#[allow(clippy::too_many_arguments)]
fn draw_hoyolab_marker(
    pm: &mut Pixmap,
    sx: f32,
    sy: f32,
    u: f32,
    icon_pm: Option<&Pixmap>,
    ui_sprites: &HashMap<MarkerUiKind, Arc<Pixmap>>,
    is_selected: bool,
    is_locked: bool,
    icon_sign: i32,
    z_level: i32,
    opacity: f32,
    mask: Option<&Mask>,
) {
    let head_cy = sy - 19.85 * u;

    // 1. Official HoYoLAB selected corner brackets (+ subtle locked-target gold accent).
    if is_selected {
        if is_locked && let Some(ring) = PathBuilder::from_circle(sx, head_cy, 19.5 * u) {
            let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
            p.set_color_rgba8(251, 191, 36, 78);
            pm.fill_path(&ring, &p, FillRule::Winding, Transform::identity(), mask);
        }
        if let Some(frame) = ui_sprites.get(&MarkerUiKind::SelectedFrame) {
            draw_scaled_sprite(
                pm,
                frame,
                [sx - 19.0 * u, head_cy - 19.0 * u, 38.0 * u, 38.0 * u],
                opacity,
                mask,
            );
        }
    }

    // 2. Official HoYoLAB pin backdrop sprite (normal vs selected).
    let bg_kind = if is_selected {
        MarkerUiKind::BgSelected
    } else {
        MarkerUiKind::BgNormal
    };
    if let Some(bg) = ui_sprites.get(&bg_kind).or_else(|| ui_sprites.get(&MarkerUiKind::BgNormal)) {
        draw_scaled_sprite(
            pm,
            bg,
            [sx - 21.0 * u, sy - 40.0 * u, 42.0 * u, 45.0 * u],
            opacity,
            mask,
        );
    } else if let Some(circle) = PathBuilder::from_circle(sx, head_cy, 15.2 * u) {
        // Brief loading placeholder before official `ui_marker_bg_normal.img` finishes caching.
        let mut fill_p = Paint {
            anti_alias: true,
            ..Default::default()
        };
        fill_p.set_color_rgba8(44, 51, 66, (232.0 * opacity).round() as u8);
        pm.fill_path(&circle, &fill_p, FillRule::Winding, Transform::identity(), mask);

        let mut rim_p = Paint {
            anti_alias: true,
            ..Default::default()
        };
        if is_selected {
            rim_p.set_color_rgba8(90, 240, 255, (235.0 * opacity).round() as u8);
        } else {
            rim_p.set_color_rgba8(204, 184, 140, (225.0 * opacity).round() as u8);
        }
        let st = Stroke {
            width: (2.2 * u).max(1.2),
            ..Stroke::default()
        };
        pm.stroke_path(&circle, &rim_p, &st, Transform::identity(), mask);
    }

    // 3. Official circular-clipped category / waypoint icon dead-center inside pin head.
    if let Some(icon) = icon_pm {
        let sz = 28.4 * u;
        draw_scaled_sprite(
            pm,
            icon,
            [sx - sz * 0.5, head_cy - sz * 0.5, sz, sz],
            opacity,
            mask,
        );
    }

    // 4. Official HoYoLAB bottom-right sub-badge (`icon_sign`: 1 = underground layered, 3 = waterhole).
    let sub_kind = match icon_sign {
        1 => Some(if is_selected {
            MarkerUiKind::UndergroundMarkSelected
        } else {
            MarkerUiKind::UndergroundMark
        }),
        3 => Some(if is_selected {
            MarkerUiKind::WaterholeMarkSelected
        } else {
            MarkerUiKind::WaterholeMark
        }),
        _ => None,
    };
    if let Some(sk) = sub_kind
        && let Some(sub_sp) = ui_sprites.get(&sk)
    {
        draw_scaled_sprite(
            pm,
            sub_sp,
            [sx + 6.5 * u, sy - 20.0 * u, 14.0 * u, 14.0 * u],
            opacity,
            mask,
        );
    }

    // 5. Official HoYoLAB left-side Z-level vertical bar (`z_level`: 1 = upper, 2 = middle, 3 = lower).
    let z_kind = match z_level {
        1 => Some(MarkerUiKind::ZLevelUpper),
        2 => Some(MarkerUiKind::ZLevelMiddle),
        3 => Some(MarkerUiKind::ZLevelLower),
        _ => None,
    };
    if let Some(zk) = z_kind
        && let Some(z_sp) = ui_sprites.get(&zk)
    {
        draw_scaled_sprite(
            pm,
            z_sp,
            [sx - 19.0 * u, sy - 35.0 * u, 9.0 * u, 25.0 * u],
            opacity,
            mask,
        );
    }
}

/// Draws a glowing M-Map-style double ring around an active Route stop or Teleport Waypoint.
fn draw_target_beacon_ring(
    pm: &mut Pixmap,
    cx: f32,
    cy: f32,
    radius: f32,
    accent: [u8; 4],
    mask: Option<&Mask>,
) {
    let [r, g, b, _] = accent;
    for (rad, col, width) in [
        (radius + 3.0, [r, g, b, 85], 6.0),
        (radius, [2, 6, 23, 165], 3.4),
        (radius, [255, 255, 255, 240], 1.8),
        (radius + 3.8, [r, g, b, 235], 1.8),
    ] {
        if let Some(circle) = PathBuilder::from_circle(cx, cy, rad) {
            let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
            p.set_color_rgba8(col[0], col[1], col[2], col[3]);
            let st = Stroke {
                width,
                ..Stroke::default()
            };
            pm.stroke_path(&circle, &p, &st, Transform::identity(), mask);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_offscreen_target_edge_arrow(
    pm: &mut Pixmap,
    cx: f32,
    cy: f32,
    tx: f32,
    ty: f32,
    wf: f32,
    hf: f32,
    is_locked: bool,
    mask: Option<&Mask>,
) {
    let dx = tx - cx;
    let dy = ty - cy;
    let dist = dx.hypot(dy);
    if dist < 1.0 {
        return;
    }
    let ux = dx / dist;
    let uy = dy / dist;
    let half_w = (wf * 0.5 - 15.0).max(12.0);
    let half_h = (hf * 0.5 - 15.0).max(12.0);
    let tx_t = if ux.abs() > 1e-4 {
        half_w / ux.abs()
    } else {
        f32::INFINITY
    };
    let ty_t = if uy.abs() > 1e-4 {
        half_h / uy.abs()
    } else {
        f32::INFINITY
    };
    let t = tx_t.min(ty_t);
    let ex = cx + ux * t;
    let ey = cy + uy * t;

    let ang = dy.atan2(dx) + std::f32::consts::FRAC_PI_2;
    let style = if is_locked {
        crate::star_dart::StarDartStyle::EDGE_GOLD
    } else {
        crate::star_dart::StarDartStyle::EDGE_CYAN
    };
    crate::star_dart::draw_star_dart(pm, ex, ey, 8.5, ang, style, Transform::identity(), mask);
}

fn draw_player_fov_and_arrow(
    pm: &mut Pixmap,
    cx: f32,
    cy: f32,
    cam_deg: f64,
    body_deg: f64,
    mask: Option<&Mask>,
) {
    // 1. Translucent cyan camera FOV cone (`0° = North`, clockwise, ~84° FOV) with antique-gold outer rim.
    let cam_rad = (cam_deg - 90.0).to_radians() as f32;
    let spread = 42.0f32.to_radians();
    for (cone_r, alpha) in [(50.0f32, 34u8), (32.0f32, 36u8)] {
        let mut cone_pb = PathBuilder::new();
        cone_pb.move_to(cx, cy);
        let steps = 12;
        for i in 0..=steps {
            let a = cam_rad - spread + (2.0 * spread) * (i as f32 / steps as f32);
            cone_pb.line_to(cx + a.cos() * cone_r, cy + a.sin() * cone_r);
        }
        cone_pb.close();
        if let Some(cone_path) = cone_pb.finish() {
            let mut p = Paint {
                anti_alias: true,
                ..Default::default()
            };
            p.set_color_rgba8(125, 240, 255, alpha);
            pm.fill_path(&cone_path, &p, FillRule::Winding, Transform::identity(), mask);
        }
    }
    let mut arc_pb = PathBuilder::new();
    for i in 0..=16 {
        let a = cam_rad - spread + (2.0 * spread) * (i as f32 / 16.0);
        let px = cx + a.cos() * 50.0;
        let py = cy + a.sin() * 50.0;
        if i == 0 {
            arc_pb.move_to(px, py);
        } else {
            arc_pb.line_to(px, py);
        }
    }
    if let Some(arc_path) = arc_pb.finish() {
        let mut gp = Paint {
            anti_alias: true,
            ..Default::default()
        };
        gp.set_color_rgba8(211, 188, 142, 175);
        let gs = Stroke {
            width: 1.2,
            ..Stroke::default()
        };
        pm.stroke_path(&arc_path, &gp, &gs, Transform::identity(), mask);
    }

    // 2. Unified symmetrical Tinh Tú (Star Dart) player arrow.
    crate::star_dart::draw_star_dart(
        pm,
        cx,
        cy,
        11.5,
        (body_deg as f32).to_radians(),
        crate::star_dart::StarDartStyle::PLAYER,
        Transform::identity(),
        mask,
    );
}

fn draw_inner_vignette(pm: &mut Pixmap, pad: f32, wf: f32, hf: f32, radius: f32, mask: Option<&Mask>) {
    if let Some(vig_path) =
        build_rounded_rect_path(pad + 1.0, pad + 1.0, wf - (pad + 1.0) * 2.0, hf - (pad + 1.0) * 2.0, radius)
    {
        let mut p = Paint {
            anti_alias: true,
            ..Default::default()
        };
        p.set_color_rgba8(8, 12, 22, 42);
        let st = Stroke {
            width: 8.0,
            ..Stroke::default()
        };
        pm.stroke_path(&vig_path, &p, &st, Transform::identity(), mask);
    }
}

fn draw_celestial_frame(pm: &mut Pixmap, pad: f32, wf: f32, hf: f32, radius: f32, edit_mode: bool) {
    // Inner subtle dark liner
    if let Some(inner_path) = build_rounded_rect_path(
        pad + 0.8,
        pad + 0.8,
        (wf - (pad + 0.8) * 2.0).max(4.0),
        (hf - (pad + 0.8) * 2.0).max(4.0),
        (radius - 0.5).max(1.0),
    ) {
        let mut p = Paint {
            anti_alias: true,
            ..Default::default()
        };
        p.set_color_rgba8(10, 14, 22, 145);
        let st = Stroke {
            width: 0.9,
            ..Stroke::default()
        };
        pm.stroke_path(&inner_path, &p, &st, Transform::identity(), None);
    }

    // Outer ultra-thin warm champagne-gold Genshin hairline (#D3BC8E)
    if let Some(border_path) =
        build_rounded_rect_path(pad, pad, (wf - pad * 2.0).max(4.0), (hf - pad * 2.0).max(4.0), radius)
    {
        let mut p = Paint {
            anti_alias: true,
            ..Default::default()
        };
        if edit_mode {
            p.set_color_rgba8(245, 208, 128, 225);
            let st = Stroke {
                width: 1.8,
                dash: tiny_skia::StrokeDash::new(vec![7.0, 4.0], 0.0),
                ..Stroke::default()
            };
            pm.stroke_path(&border_path, &p, &st, Transform::identity(), None);
        } else {
            p.set_color_rgba8(211, 188, 142, 102);
            let st = Stroke {
                width: 1.1,
                ..Stroke::default()
            };
            pm.stroke_path(&border_path, &p, &st, Transform::identity(), None);
        }
    }

    // 4 delicate corner accent ticks (or prominent resize handles in Edit Mode).
    let tick = if edit_mode { 11.0 } else { 6.0 };
    let inset = pad + 4.5;
    let corners = [
        (inset, inset, 1.0f32, 1.0f32),
        (wf - inset, inset, -1.0, 1.0),
        (inset, hf - inset, 1.0, -1.0),
        (wf - inset, hf - inset, -1.0, -1.0),
    ];
    let mut p = Paint {
        anti_alias: true,
        ..Default::default()
    };
    if edit_mode {
        p.set_color_rgba8(251, 191, 36, 235);
    } else {
        p.set_color_rgba8(236, 229, 216, 118);
    }
    let st = Stroke {
        width: if edit_mode { 2.2 } else { 1.1 },
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    for (x, y, dx, dy) in corners {
        let mut pb = PathBuilder::new();
        pb.move_to(x + dx * tick, y);
        pb.line_to(x, y);
        pb.line_to(x, y + dy * tick);
        if let Some(path) = pb.finish() {
            pm.stroke_path(&path, &p, &st, Transform::identity(), None);
        }
    }
}

/// Parses a `/lan_pins` JSON payload into a compact `Vec<RadarPin>` (called ONCE per snapshot,
/// and skipped completely when Radar is OFF).
pub fn parse_radar_pins_payload(val: &serde_json::Value) -> (i32, Vec<RadarPin>) {
    let map_id = val
        .get("map_id")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(2) as i32;
    let Some(arr) = val.get("pins").and_then(serde_json::Value::as_array) else {
        return (map_id, Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len().min(2048));
    for item in arr {
        let Some(row) = item.as_array() else {
            continue;
        };
        if row.len() < 8 {
            continue;
        }
        let id = match &row[0] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => continue,
        };
        let Some(lat) = row[1].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let Some(lng) = row[2].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let label_id = row[3].as_i64().unwrap_or(0) as i32;
        let floor_id = match &row[4] {
            serde_json::Value::String(s) if !s.trim().is_empty() && s.trim() != "0" => {
                Some(s.trim().to_owned())
            }
            serde_json::Value::Number(n) if n.as_i64().unwrap_or(0) != 0 => Some(n.to_string()),
            _ => None,
        };
        let marked = row[5].as_i64().unwrap_or(0) != 0 || row[5].as_bool().unwrap_or(false);
        let is_underground =
            row[7].as_i64().unwrap_or(0) != 0 || row[7].as_bool().unwrap_or(false);
        let icon_sign = row
            .get(8)
            .and_then(serde_json::Value::as_i64)
            .map(|v| v as i32)
            .unwrap_or(if is_underground { 1 } else { 0 });
        let z_level = row
            .get(9)
            .and_then(serde_json::Value::as_i64)
            .map(|v| v as i32)
            .unwrap_or(0);
        out.push(RadarPin {
            id,
            lat,
            lng,
            label_id,
            floor_id,
            marked,
            is_underground,
            icon_sign,
            z_level,
        });
    }
    (map_id, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::multimap::{DetailV2Meta, FloorEntry, FloorOverlayMeta, MapMeta, PointGroupEntry};
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::collections::BTreeMap;
    use std::io::Cursor;

    struct MockRadarAssets {
        db: Arc<MultimapDb>,
        tile_png: Vec<u8>,
        icon_png: Vec<u8>,
    }

    impl MockRadarAssets {
        fn new() -> Self {
            let mut maps = BTreeMap::new();
            maps.insert(
                2,
                MapMeta {
                    id: 2,
                    name: "Teyvat".to_owned(),
                    priority: 1,
                    detail_v2: Some(DetailV2Meta {
                        map_version: "vtest".to_owned(),
                        total_size: [16_384.0, 16_384.0],
                        padding: [0.0, 0.0],
                        origin: [8_192.0, 8_192.0],
                        original_map_size: None,
                        min_zoom: -4,
                        max_zoom: 0,
                    }),
                    point_groups: vec![PointGroupEntry {
                        id: 10,
                        map_id: 2,
                        name: "Cave".to_owned(),
                        g_floor_name: "G".to_owned(),
                        default_floor_index: 0,
                        floors: vec![FloorEntry {
                            id: 501,
                            floor_name: "B1".to_owned(),
                            floor_name_short: "B1".to_owned(),
                            overlay: Some(FloorOverlayMeta {
                                url: "https://act-webstatic.hoyoverse.com/ug.png".to_owned(),
                                l_x: -100.0,
                                l_y: -80.0,
                                r_x: 100.0,
                                r_y: 80.0,
                                reflect_y: Some(true),
                                ..Default::default()
                            }),
                            index: 0,
                        }],
                    }],
                    ..Default::default()
                },
            );
            let mut tile_buf = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(RgbaImage::from_pixel(256, 256, Rgba([40, 120, 160, 255])))
                .write_to(&mut tile_buf, ImageFormat::Png)
                .unwrap();
            let mut icon_buf = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 32, Rgba([255, 220, 80, 255])))
                .write_to(&mut icon_buf, ImageFormat::Png)
                .unwrap();
            Self {
                db: Arc::new(MultimapDb {
                    version: "4.0.0".to_owned(),
                    maps,
                }),
                tile_png: tile_buf.into_inner(),
                icon_png: icon_buf.into_inner(),
            }
        }
    }

    impl RadarAssetSource for MockRadarAssets {
        fn maps_db(&self) -> Arc<MultimapDb> {
            Arc::clone(&self.db)
        }
        fn tile_generation(&self) -> u64 {
            1
        }
        fn icon_generation(&self) -> u64 {
            1
        }
        fn get_color_tile_bytes(&self, _m: i32, _z: i32, _tx: i32, _ty: i32) -> Option<Vec<u8>> {
            Some(self.tile_png.clone())
        }
        fn prefetch_color_tiles_async(&self, _m: i32, _tiles: &[(i32, i32, i32)]) {}
        fn get_ug_overlay_bytes(&self, _m: i32, _g: i32, _f: i32) -> Option<Vec<u8>> {
            Some(self.tile_png.clone())
        }
        fn get_icon_bytes_or_request(&self, _m: i32, _lid: i32) -> Option<Vec<u8>> {
            Some(self.icon_png.clone())
        }
        fn get_marker_ui_asset_or_request(&self, _key: &str) -> Option<Vec<u8>> {
            Some(self.icon_png.clone())
        }
    }

    #[test]
    fn default_bottom_right_rect_covers_uid_and_avoids_cv_zones() {
        for (gw, gh) in [(1920, 1080), (2560, 1440), (3440, 1440)] {
            let game = RectI32 {
                x: 0,
                y: 0,
                width: gw,
                height: gh,
            };
            let r = compute_default_radar_rect(game);
            assert!(
                !radar_overlaps_cv_zones(r, game),
                "bottom-right default must not overlap top-left CV zones at {gw}x{gh}"
            );
            // Covers Genshin UID region (~x >= 0.85*gw, y >= 0.97*gh)
            assert!(r.x <= (gw as f64 * 0.84) as i32);
            assert!(r.x + r.width >= (gw as f64 * 0.98) as i32);
            assert!(r.y + r.height >= (gh as f64 * 0.99) as i32);

            // Clamping against full monitor bounds (`rcMonitor`) preserves bottom-edge placement
            // over the taskbar/UID strip (unlike `rcWork`, which pushed it up by ~48 px).
            let flush_bottom = RectI32 {
                x: gw - 602,
                y: gh - 551,
                width: 602,
                height: 551,
            };
            let rel = radar_rect_to_relative(flush_bottom, game).expect("rel");
            let resolved = resolve_radar_rect(game, Some(rel), Some((0, 0, gw, gh)));
            assert_eq!(resolved.y + resolved.height, gh);
        }

        // Dragging Radar onto top-left minimap triggers CV overlap guard!
        let game = RectI32 {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let top_left = RectI32 {
            x: 40,
            y: 20,
            width: 300,
            height: 260,
        };
        assert!(radar_overlaps_cv_zones(top_left, game));
    }

    #[test]
    fn north_up_projection_and_underground_reflect_y_box_match_hoyolab() {
        let (sx, sy) = world_to_radar_px(100.0, -50.0, 0.0, 0.0, 0.0, 200.0, 150.0);
        assert!((sx - 300.0).abs() < 1e-4);
        assert!((sy - 100.0).abs() < 1e-4);

        let assets = MockRadarAssets::new();
        let rec = assets.db.floor_overlays_for_map(2).pop().unwrap();
        let (dx, dy, dw, dh, reflect_y) =
            underground_overlay_screen_box(&rec, 0.0, 0.0, 0.0, 200.0, 150.0).unwrap();
        assert!(reflect_y);
        assert!((dx - 100.0).abs() < 1e-4);
        assert!((dy - 70.0).abs() < 1e-4);
        assert!((dw - 200.0).abs() < 1e-4);
        assert!((dh - 160.0).abs() < 1e-4);
    }

    #[test]
    fn parse_radar_pins_payload_and_hoyolab_marker_rendering_work() {
        let payload = serde_json::json!({
            "map_id": 2,
            "names": ["Chest"],
            "pins": [
                ["1001", -12.5, 44.2, 17, null, 0, 0, 0],
                ["1002", -15.0, 48.0, 17, "501", 1, 0, 1],
                ["1003", -20.0, 50.0, 17, "502", 0, 0, 1, 3, 2]
            ]
        });
        let (mid, pins) = parse_radar_pins_payload(&payload);
        assert_eq!(mid, 2);
        assert_eq!(pins.len(), 3);
        assert!(!pins[0].marked && !pins[0].is_underground && pins[0].icon_sign == 0 && pins[0].z_level == 0);
        assert!(pins[1].marked && pins[1].is_underground && pins[1].floor_id.as_deref() == Some("501") && pins[1].icon_sign == 1);
        assert!(pins[2].is_underground && pins[2].icon_sign == 3 && pins[2].z_level == 2);

        let assets = MockRadarAssets::new();
        let ico = decode_circular_category_icon(&assets.icon_png).expect("circular icon");
        assert_eq!((ico.width(), ico.height()), (48, 48));
        // Corner pixel outside circular clip must stay transparent.
        assert_eq!(ico.data()[3], 0);

        let mut sprites = HashMap::new();
        for &kind in MarkerUiKind::ALL {
            sprites.insert(kind, Arc::clone(&ico));
        }
        let mut canvas = Pixmap::new(96, 96).unwrap();
        draw_hoyolab_marker(
            &mut canvas,
            48.0,
            64.0,
            RADAR_PIN_SCALE,
            Some(&ico),
            &sprites,
            true,
            true,
            1,
            2,
            1.0,
            None,
        );
        assert!(canvas.data().as_chunks::<4>().0.iter().any(|px| px[3] > 0));

        // Critically-damped spring converges monotonically without overshooting target.
        let (mut pos, mut vel) = (0.0f64, 0.0f64);
        for _ in 0..40 {
            (pos, vel) = step_critically_damped_axis(pos, vel, 100.0, POS_SPRING_OMEGA, 1.0 / 60.0);
            assert!(pos <= 100.0001, "critically damped spring must not overshoot: {pos}");
        }
        assert!((pos - 100.0).abs() < 0.5, "should converge closely: {pos}");
    }

    #[test]
    fn radar_controller_offscreen_render_and_zero_overhead_teardown() {
        let assets = Arc::new(MockRadarAssets::new());
        let ctrl = RadarMapController::new_with_offset(
            assets,
            None,
            DEFAULT_RADAR_ZOOM,
            gh_core::config::DEFAULT_RADAR_OPACITY,
            false,
            None,
            crate::layered::OFFSCREEN_TEST_OFFSET,
        );
        assert!(ctrl.is_running());

        ctrl.update(RadarScene {
            game_rect: RectI32 {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            map_id: 2,
            world_x: 10.0,
            world_y: -20.0,
            vx: 5.0,
            vy: 0.0,
            body_heading: Some(45.0),
            camera_heading: Some(60.0),
            is_underground: true,
            group_id: Some(10),
            floor_id: Some(501),
            floor_short: "B1".to_owned(),
            pins: Arc::new(vec![RadarPin {
                id: "99".to_owned(),
                lat: -10.0,
                lng: 30.0,
                label_id: 17,
                floor_id: Some("501".to_owned()),
                marked: false,
                is_underground: true,
                icon_sign: 1,
                z_level: 0,
            }]),
            excluded_ids: Arc::new(HashSet::new()),
            target: Some(RadarTargetHint {
                id: "99".to_owned(),
                lat: -10.0,
                lng: 30.0,
                is_locked: true,
                is_underground: true,
                floor_id: Some("501".to_owned()),
                is_route_stop: true,
                accent: Some([253, 224, 71, 235]),
            }),
            route_anchor: None,
            route_legs: Arc::new(vec![RadarRouteLeg {
                from_lat: -20.0,
                from_lng: 10.0,
                to_lat: -10.0,
                to_lng: 30.0,
                done: false,
                step_index: 0,
                color: [253, 224, 71, 235],
                emphasized: true,
            }]),
            capture_visible: false,
            edit_hint: "Edit".to_owned(),
        });
        assert!(
            ctrl.host.wait_until_state(Duration::from_secs(15), || ctrl.is_visible()),
            "timed out waiting for offscreen radar to present"
        );
        ctrl.close();
        assert!(!ctrl.is_running());
        assert_eq!(ctrl.raw_hwnd(), None);
        assert!(!ctrl.is_visible());
    }

    #[test]
    fn headless_radar_renderer_draws_non_empty_pixmap_without_hwnd() {
        let assets: Arc<dyn RadarAssetSource> = Arc::new(MockRadarAssets::new());
        let mut renderer = HeadlessRadarRenderer::new(assets);
        let scene = RadarScene {
            game_rect: RectI32 {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
            map_id: 2,
            world_x: 100.0,
            world_y: -50.0,
            vx: 0.0,
            vy: 0.0,
            body_heading: Some(45.0),
            camera_heading: Some(60.0),
            is_underground: false,
            group_id: None,
            floor_id: None,
            floor_short: String::new(),
            pins: Arc::new(Vec::new()),
            excluded_ids: Arc::new(HashSet::new()),
            target: None,
            route_anchor: None,
            route_legs: Arc::new(Vec::new()),
            capture_visible: false,
            edit_hint: String::new(),
        };
        let (w, h, rgba) = renderer
            .render_scene_rgba(&scene, 420, 300, 0.0, false)
            .expect("headless radar must render pixmap");
        assert_eq!((w, h), (420, 300));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        let opaque_pixels = rgba.as_chunks::<4>().0.iter().filter(|px| px[3] > 200).count();
        assert!(
            opaque_pixels > (w * h / 2) as usize,
            "expected solid radar card interior, got only {opaque_pixels} opaque pixels"
        );
    }
}
