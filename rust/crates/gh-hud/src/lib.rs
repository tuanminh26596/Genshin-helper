//! Overlay HUD & Vietnamese PNG annotation crate: Win32 `UpdateLayeredWindow` +
//! `tiny-skia` + `cosmic-text` (pre-baked 2-layer `<0.4 ms/frame`, zero D3D/DXGI contention).
//!
//! Acyclic dependency: depends ONLY on `gh-core` + `gh-win32`.

pub mod diag_annotate;
pub mod fonts;
pub mod layered;
#[cfg(target_os = "linux")]
pub mod layered_linux;
#[cfg(target_os = "linux")]
pub mod wayland_overlay;
pub mod mmap_overlay;
pub mod nav_hud;
pub mod radar_map;
pub(crate) mod star_dart;
pub mod toast;

pub use radar_map::{
    RADAR_LEASE_MS, RADAR_MAX_SIZE, RADAR_MIN_SIZE, RadarAssetSource, RadarLayoutCallback,
    RadarMapController, RadarPin, RadarRouteLeg, RadarScene, RadarTargetHint,
    compute_default_radar_rect, parse_radar_pins_payload, radar_overlaps_cv_zones,
    radar_rect_to_relative, resolve_radar_rect, underground_overlay_screen_box, world_to_radar_px,
};

pub use mmap_overlay::{
    MMapEdgeArrow, MMapGuideScene, MMapGuideStage, MMapOverlayController, MMapOverlayMode, MMapPath,
    MMapTeleportMarker, parse_hex_rgba,
};

pub use nav_hud::{
    BADGE_COUNT, BADGE_STEP_DEG, BEHIND_DEG, COMPACT_DIST_THRESHOLD_M, CircularAngleSmoother,
    EDIT_TIMEOUT_S, GUIDE_IMAGE_MAX, GUIDE_ZOOM_MODES, GuideZoomMode, HudPalette, HudRenderMeta,
    HudRenderStats, LOCK_ON_DEG, LabelResolver, MAX_BADGE_CACHE_BYTES, MAX_CARD_CACHE_ENTRIES,
    MAX_GUIDE_CACHE_ENTRIES, MAX_RADAR_CACHE_ENTRIES, MAX_THUMB_CACHE_ENTRIES, NavHudConfig,
    NavHudImageCache, NavHudRenderer, NavMiniHud, NavRenderOptions, POSITION_MODES,
    POSITION_NOISE_M, PaletteColors, PositionChangedCallback, PositionMode, RADAR_RANGE_M,
    RENDER_TICK_MS, ScalarSmoother, SpringAngleSmoother, THUMB_SIZE, TIMER_RENDER, WM_APP_CLOSE,
    WM_APP_EDIT, WM_APP_HIDE, WM_APP_UPDATE, clean_floor_label, compute_hud_position,
    finish_drag_to_custom, format_floor_tag, noise_deadband_deg, norm_custom_pos,
    norm_guide_zoom_mode, norm_position_mode,
};

use thiserror::Error;

/// Unified error type for `gh-hud` surface, font, toast, and annotation operations.
#[derive(Debug, Error)]
pub enum HudError {
    #[error("core error: {0}")]
    Core(#[from] gh_core::CoreError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
    #[error("Win32 error: {0}")]
    Win32(String),
    #[error("invalid pixel buffer: {0}")]
    InvalidBuffer(String),
    #[error("render error: {0}")]
    Render(String),
    #[error("PNG encode error: {0}")]
    PngEncode(String),
}

pub type HudResult<T> = Result<T, HudError>;
