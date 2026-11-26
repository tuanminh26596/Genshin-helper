//! 72-tile O(1) LRU terrain tile cache + `diag(1, -1, 1)` underground Y-reflect patch extractor
//! (`src/tile_cache.py`).

use crate::image_io::{DecodedImage, ImageReadMode, decode_image_bytes, read_image};
use crate::locator::TerrainRaster;
use crate::ops::color::{bgr_px_to_gray, bgr_to_gray};
use crate::ops::resize::resize_u8;
use crate::ops::warp::warp_affine_u8;
use crate::ops::{BorderMode, CvError, CvResult, Interpolation};
use gh_core::assets::resolve_data_asset;
use gh_core::frame::{FrameView, PixelFormat};
use gh_core::multimap::FloorOverlayMeta;
use gh_core::npz::{NpyData, NpzArchive};
use gh_core::paths::AppPaths;
use gh_core::traits::{TileProvider, UndergroundOverlaySource};
use ndarray::{Array2, ArrayView2, s};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const LRU_TILE_CAPACITY: usize = 72;
pub const DEFAULT_MAX_TILES: usize = 16;
pub const DEFAULT_TILE_SIZE: usize = 256;
pub const MIN_TILE_SIZE: usize = 16;
pub const MIN_PATCH_DIM: usize = 16;
pub const MAX_PATCH_DIM: usize = 16_384;
pub const MAX_MOSAIC_TILES: i32 = 64;
pub const ALPHA_TRANSPARENT_CUTOFF: u8 = 16;
pub const SCALE_AREA_THRESHOLD: f64 = 1.25;
const MAX_AFFINE_COORD: f64 = 1e15;

#[inline]
fn affine_corners_safe(m: &[f64; 6], out_size: usize) -> bool {
    if out_size == 0 || !m.iter().all(|v| v.is_finite()) {
        return false;
    }
    let max_d = (out_size - 1) as f64;
    for dy in [0.0, max_d] {
        for dx in [0.0, max_d] {
            let sx = m[0] * dx + m[1] * dy + m[2];
            let sy = m[3] * dx + m[4] * dy + m[5];
            if !sx.is_finite()
                || !sy.is_finite()
                || sx.abs() > MAX_AFFINE_COORD
                || sy.abs() > MAX_AFFINE_COORD
            {
                return false;
            }
        }
    }
    true
}

pub const UNDERGROUND_Y_REFLECT: [[f64; 3]; 3] = [
    [1.0, 0.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, 1.0],
];

/// `(map_id, zoom, tx, ty)` tile key.
pub type TileKey = (i32, i32, i32, i32);
/// 3x3 homogeneous affine matrix in row-major `[row][col]` order.
pub type Mat3 = [[f64; 3]; 3];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatchMeta {
    pub origin_wx: f64,
    pub origin_wy: f64,
    pub px_per_unit: f64,
}

impl PatchMeta {
    #[inline]
    pub fn pixel_to_world(&self, px: f64, py: f64) -> (f64, f64) {
        (
            self.origin_wx + (px + 0.5) / self.px_per_unit,
            self.origin_wy + (py + 0.5) / self.px_per_unit,
        )
    }

    #[inline]
    pub fn world_to_pixel(&self, wx: f64, wy: f64) -> (f64, f64) {
        (
            (wx - self.origin_wx) * self.px_per_unit - 0.5,
            (wy - self.origin_wy) * self.px_per_unit - 0.5,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileMapMeta {
    pub tx_start: i32,
    pub ty_start: i32,
    pub native_z: i32,
    pub orig_x: f64,
    pub orig_y: f64,
}

impl From<[f64; 5]> for TileMapMeta {
    fn from(m: [f64; 5]) -> Self {
        Self {
            tx_start: m[0].round_ties_even() as i32,
            ty_start: m[1].round_ties_even() as i32,
            native_z: m[2].round_ties_even() as i32,
            orig_x: m[3],
            orig_y: m[4],
        }
    }
}

/// Directional HoYoLAB underground floor overlay bounds `(lx, ly, rx, ry)`.
///
/// Always construct from `FloorOverlayMeta` (NOT `FloorOverlayRecord::lx/rx/ly/ry`
/// which are min/max sorted and discard `l_y > r_y` orientation).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayBounds {
    pub lx: f64,
    pub ly: f64,
    pub rx: f64,
    pub ry: f64,
    pub reflect_y_hint: Option<bool>,
}

impl OverlayBounds {
    pub const fn new(lx: f64, ly: f64, rx: f64, ry: f64) -> Self {
        Self {
            lx,
            ly,
            rx,
            ry,
            reflect_y_hint: None,
        }
    }

    pub const fn with_reflect_y(mut self, reflect_y: Option<bool>) -> Self {
        self.reflect_y_hint = reflect_y;
        self
    }

    pub fn from_floor_overlay_meta(meta: &FloorOverlayMeta) -> Self {
        let has_dir = meta.l_x.is_finite()
            && meta.l_y.is_finite()
            && meta.r_x.is_finite()
            && meta.r_y.is_finite()
            && (meta.r_x - meta.l_x).abs() >= 1e-9
            && (meta.r_y - meta.l_y).abs() >= 1e-9;
        if has_dir {
            return Self {
                lx: meta.l_x,
                ly: meta.l_y,
                rx: meta.r_x,
                ry: meta.r_y,
                reflect_y_hint: meta.reflect_y,
            };
        }
        if let (Some(min_lng), Some(min_lat), Some(max_lng), Some(max_lat)) =
            (meta.min_lng, meta.min_lat, meta.max_lng, meta.max_lat)
            && min_lng.is_finite()
            && min_lat.is_finite()
            && max_lng.is_finite()
            && max_lat.is_finite()
        {
            return Self {
                lx: min_lng,
                ly: min_lat,
                rx: max_lng,
                ry: max_lat,
                reflect_y_hint: meta.reflect_y,
            };
        }
        Self {
            lx: meta.l_x,
            ly: meta.l_y,
            rx: meta.r_x,
            ry: meta.r_y,
            reflect_y_hint: meta.reflect_y,
        }
    }
}

impl From<&FloorOverlayMeta> for OverlayBounds {
    fn from(meta: &FloorOverlayMeta) -> Self {
        Self::from_floor_overlay_meta(meta)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub tile_count: usize,
    pub approx_ram_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub disk_loads: u64,
    pub provider_loads: u64,
}

#[derive(Clone)]
pub struct TileCacheConfig {
    pub max_tiles: usize,
    pub tile_size: usize,
    pub npz_path: Option<PathBuf>,
    pub use_default_npz: bool,
    pub disk_cache_dir: Option<PathBuf>,
    pub terrain_data: BTreeMap<i32, Arc<TerrainRaster>>,
    pub tile_provider: Option<Arc<dyn TileProvider>>,
    pub async_fetch: bool,
}

impl std::fmt::Debug for TileCacheConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileCacheConfig")
            .field("max_tiles", &self.max_tiles)
            .field("tile_size", &self.tile_size)
            .field("npz_path", &self.npz_path)
            .field("use_default_npz", &self.use_default_npz)
            .field("disk_cache_dir", &self.disk_cache_dir)
            .field("terrain_data_keys", &self.terrain_data.keys().collect::<Vec<_>>())
            .field("has_tile_provider", &self.tile_provider.is_some())
            .field("async_fetch", &self.async_fetch)
            .finish()
    }
}

impl Default for TileCacheConfig {
    fn default() -> Self {
        Self {
            max_tiles: LRU_TILE_CAPACITY,
            tile_size: DEFAULT_TILE_SIZE,
            npz_path: None,
            use_default_npz: true,
            disk_cache_dir: None,
            terrain_data: BTreeMap::new(),
            tile_provider: None,
            async_fetch: true,
        }
    }
}

pub fn default_disk_tile_dir() -> PathBuf {
    AppPaths::resolve().tiles_dir
}

pub fn decoded_image_to_gray_alpha_zeroed(img: &DecodedImage) -> CvResult<Array2<u8>> {
    match img {
        DecodedImage::Gray(arr) => {
            if arr.is_empty() {
                return Err(CvError::EmptyInput("empty gray image".into()));
            }
            if arr.is_standard_layout() {
                Ok(arr.clone())
            } else {
                Ok(arr.as_standard_layout().into_owned())
            }
        }
        DecodedImage::Bgr(arr) => {
            if arr.is_empty() {
                return Err(CvError::EmptyInput("empty bgr image".into()));
            }
            bgr_to_gray(arr.view())
        }
        DecodedImage::Bgra(arr) => {
            let (h, w, c) = arr.dim();
            if h == 0 || w == 0 || c < 4 {
                return Err(CvError::EmptyInput("invalid bgra image".into()));
            }
            let mut out = Array2::<u8>::zeros((h, w));
            for y in 0..h {
                for x in 0..w {
                    let a = arr[[y, x, 3]];
                    if a < ALPHA_TRANSPARENT_CUTOFF {
                        out[[y, x]] = 0;
                    } else {
                        out[[y, x]] = bgr_px_to_gray(arr[[y, x, 0]], arr[[y, x, 1]], arr[[y, x, 2]]);
                    }
                }
            }
            Ok(out)
        }
    }
}

pub fn frame_view_to_gray_alpha_zeroed(view: FrameView<'_>) -> CvResult<Array2<u8>> {
    let w = view.width as usize;
    let h = view.height as usize;
    if w == 0 || h == 0 {
        return Err(CvError::EmptyInput("empty FrameView".into()));
    }
    let ch = view.format.channels();
    if view.stride < w * ch {
        return Err(CvError::ShapeMismatch("FrameView stride too small".into()));
    }
    let needed = (h - 1) * view.stride + w * ch;
    if view.data.len() < needed {
        return Err(CvError::ShapeMismatch("FrameView buffer truncated".into()));
    }
    let mut out = Array2::<u8>::zeros((h, w));
    for y in 0..h {
        let row = &view.data[y * view.stride..y * view.stride + w * ch];
        match view.format {
            PixelFormat::Gray8 => {
                out.row_mut(y)
                    .as_slice_mut()
                    .expect("contiguous")
                    .copy_from_slice(row);
            }
            PixelFormat::Bgr8 => {
                for x in 0..w {
                    let px = &row[x * 3..];
                    out[[y, x]] = bgr_px_to_gray(px[0], px[1], px[2]);
                }
            }
            PixelFormat::Bgra8 => {
                for x in 0..w {
                    let px = &row[x * 4..];
                    out[[y, x]] = if px[3] < ALPHA_TRANSPARENT_CUTOFF {
                        0
                    } else {
                        bgr_px_to_gray(px[0], px[1], px[2])
                    };
                }
            }
            PixelFormat::Rgba8 => {
                for x in 0..w {
                    let px = &row[x * 4..];
                    out[[y, x]] = if px[3] < ALPHA_TRANSPARENT_CUTOFF {
                        0
                    } else {
                        bgr_px_to_gray(px[2], px[1], px[0])
                    };
                }
            }
        }
    }
    Ok(out)
}

/// Decodes encoded overlay PNG/WebP bytes preserving alpha so transparent `< 16` pixels are zeroed.
pub fn overlay_bytes_to_gray(bytes: &[u8]) -> CvResult<Array2<u8>> {
    let decoded = decode_image_bytes(bytes, ImageReadMode::Unchanged)?;
    decoded_image_to_gray_alpha_zeroed(&decoded)
}

#[inline]
fn mat3_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0f64; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = a[r][0] * b[0][c] + a[r][1] * b[1][c] + a[r][2] * b[2][c];
        }
    }
    out
}

/// Builds the 3x3 homogeneous affine matrix mapping underground overlay pixel `(u, v, 1)`
/// (pixel-center convention) into Genshin-Helper surface world coordinates `(wx, wy, 1)`
/// where `+X = East` and `+Y = South`.
pub fn compose_underground_local_to_surface(
    bounds: &OverlayBounds,
    img_w: usize,
    img_h: usize,
    world_y: Option<f64>,
    half_extent_world: f64,
    reflect_y: Option<bool>,
) -> CvResult<(Mat3, bool)> {
    if img_w == 0 || img_h == 0 {
        return Err(CvError::InvalidArgument(format!(
            "invalid overlay dimensions: ({img_w}, {img_h})"
        )));
    }
    let lx = bounds.lx;
    let ly = bounds.ly;
    let rx = bounds.rx;
    let ry = bounds.ry;
    if !lx.is_finite()
        || !ly.is_finite()
        || !rx.is_finite()
        || !ry.is_finite()
        || (rx - lx).abs() < 1e-9
        || (ry - ly).abs() < 1e-9
    {
        return Err(CvError::InvalidArgument(format!(
            "degenerate overlay bounds: ({lx}, {ly}) -> ({rx}, {ry})"
        )));
    }

    let sx = (rx - lx) / (img_w as f64);
    let sy = (ry - ly) / (img_h as f64);
    let local_to_world: Mat3 = [
        [sx, 0.0, lx + 0.5 * sx],
        [0.0, sy, ly + 0.5 * sy],
        [0.0, 0.0, 1.0],
    ];

    let mut effective_reflect = reflect_y.or(bounds.reflect_y_hint);
    if effective_reflect.is_none()
        && let Some(wy) = world_y
    {
        let (min_y_raw, max_y_raw) = if ly <= ry { (ly, ry) } else { (ry, ly) };
        let margin = half_extent_world.max(0.0);
        let in_raw = (min_y_raw - margin) <= wy && wy <= (max_y_raw + margin);
        let in_neg = (min_y_raw - margin) <= (-wy) && (-wy) <= (max_y_raw + margin);
        effective_reflect = Some((!in_raw) && in_neg);
    }

    let applied_reflect = effective_reflect.unwrap_or(false);
    let local_to_surface = if applied_reflect {
        mat3_mul(&UNDERGROUND_Y_REFLECT, &local_to_world)
    } else {
        local_to_world
    };
    Ok((local_to_surface, applied_reflect))
}

pub fn extract_underground_patch_gray(
    floor_overlay_gray: ArrayView2<'_, u8>,
    bounds: &OverlayBounds,
    world_x: f64,
    world_y: f64,
    half_extent_world: f64,
    target_px_per_unit: f64,
    reflect_y: Option<bool>,
) -> (Option<Array2<u8>>, PatchMeta) {
    let default_meta = PatchMeta {
        origin_wx: world_x,
        origin_wy: world_y,
        px_per_unit: target_px_per_unit,
    };
    if half_extent_world <= 0.0
        || target_px_per_unit <= 0.0
        || !world_x.is_finite()
        || !world_y.is_finite()
        || !half_extent_world.is_finite()
        || !target_px_per_unit.is_finite()
    {
        return (None, default_meta);
    }

    let raw_out_f = 2.0 * half_extent_world * target_px_per_unit;
    let out_size_f = if raw_out_f.is_finite() {
        raw_out_f.round_ties_even().max(MIN_PATCH_DIM as f64)
    } else {
        f64::INFINITY
    };
    let eff_half = (0.5 * out_size_f) / target_px_per_unit;
    let origin_wx = world_x - eff_half;
    let origin_wy = world_y - eff_half;
    let meta_out = PatchMeta {
        origin_wx,
        origin_wy,
        px_per_unit: target_px_per_unit,
    };

    if !out_size_f.is_finite()
        || !eff_half.is_finite()
        || out_size_f > MAX_PATCH_DIM as f64
        || !origin_wx.is_finite()
        || !origin_wy.is_finite()
    {
        return (None, meta_out);
    }
    let out_size = out_size_f as usize;

    let (img_h, img_w) = floor_overlay_gray.dim();
    if img_h == 0 || img_w == 0 {
        return (None, meta_out);
    }

    let Ok((local_to_surface, _)) = compose_underground_local_to_surface(
        bounds,
        img_w,
        img_h,
        Some(world_y),
        eff_half,
        reflect_y,
    ) else {
        return (None, meta_out);
    };

    let wf = img_w as f64;
    let hf = img_h as f64;
    let corners_local = [
        [-0.5, -0.5],
        [wf - 0.5, -0.5],
        [wf - 0.5, hf - 0.5],
        [-0.5, hf - 0.5],
    ];
    let mut ov_min_x = f64::INFINITY;
    let mut ov_max_x = f64::NEG_INFINITY;
    let mut ov_min_y = f64::INFINITY;
    let mut ov_max_y = f64::NEG_INFINITY;
    for [u, v] in corners_local {
        let sx = local_to_surface[0][0] * u + local_to_surface[0][1] * v + local_to_surface[0][2];
        let sy = local_to_surface[1][0] * u + local_to_surface[1][1] * v + local_to_surface[1][2];
        if !sx.is_finite() || !sy.is_finite() {
            return (None, meta_out);
        }
        ov_min_x = ov_min_x.min(sx);
        ov_max_x = ov_max_x.max(sx);
        ov_min_y = ov_min_y.min(sy);
        ov_max_y = ov_max_y.max(sy);
    }

    let q_min_x = origin_wx;
    let q_max_x = origin_wx + 2.0 * eff_half;
    let q_min_y = origin_wy;
    let q_max_y = origin_wy + 2.0 * eff_half;
    if !q_max_x.is_finite()
        || !q_max_y.is_finite()
        || q_max_x <= ov_min_x
        || q_min_x >= ov_max_x
        || q_max_y <= ov_min_y
        || q_min_y >= ov_max_y
    {
        return (None, meta_out);
    }

    let l00 = local_to_surface[0][0];
    let l11 = local_to_surface[1][1];
    if !l00.is_finite() || !l11.is_finite() || l00.abs() < 1e-12 || l11.abs() < 1e-12 {
        return (None, meta_out);
    }
    let surface_to_local: Mat3 = [
        [1.0 / l00, 0.0, -local_to_surface[0][2] / l00],
        [0.0, 1.0 / l11, -local_to_surface[1][2] / l11],
        [0.0, 0.0, 1.0],
    ];
    let inv_s = 1.0 / target_px_per_unit;
    if !inv_s.is_finite() {
        return (None, meta_out);
    }
    let patch_to_surface: Mat3 = [
        [inv_s, 0.0, origin_wx + 0.5 * inv_s],
        [0.0, inv_s, origin_wy + 0.5 * inv_s],
        [0.0, 0.0, 1.0],
    ];
    let patch_to_local = mat3_mul(&surface_to_local, &patch_to_surface);
    let m = [
        patch_to_local[0][0],
        patch_to_local[0][1],
        patch_to_local[0][2],
        patch_to_local[1][0],
        patch_to_local[1][1],
        patch_to_local[1][2],
    ];
    if !affine_corners_safe(&m, out_size) {
        return (None, meta_out);
    }
    let src_scale = patch_to_local[0][0].abs().max(patch_to_local[1][1].abs());
    let interp = if src_scale > SCALE_AREA_THRESHOLD {
        Interpolation::Area
    } else {
        Interpolation::Linear
    };
    let patch = warp_affine_u8(
        floor_overlay_gray,
        &m,
        out_size,
        out_size,
        interp,
        true,
        BorderMode::Constant(0.0),
    )
    .ok();
    (patch, meta_out)
}

pub fn extract_underground_patch(
    floor_overlay: Option<&DecodedImage>,
    bounds: &OverlayBounds,
    world_x: f64,
    world_y: f64,
    half_extent_world: f64,
    target_px_per_unit: f64,
    reflect_y: Option<bool>,
) -> (Option<Array2<u8>>, PatchMeta) {
    let default_meta = PatchMeta {
        origin_wx: world_x,
        origin_wy: world_y,
        px_per_unit: target_px_per_unit,
    };
    let Some(img) = floor_overlay else {
        return (None, default_meta);
    };
    let Ok(gray) = decoded_image_to_gray_alpha_zeroed(img) else {
        return (None, default_meta);
    };
    extract_underground_patch_gray(
        gray.view(),
        bounds,
        world_x,
        world_y,
        half_extent_world,
        target_px_per_unit,
        reflect_y,
    )
}

pub fn extract_underground_patch_view(
    floor_overlay: Option<FrameView<'_>>,
    bounds: &OverlayBounds,
    world_x: f64,
    world_y: f64,
    half_extent_world: f64,
    target_px_per_unit: f64,
    reflect_y: Option<bool>,
) -> (Option<Array2<u8>>, PatchMeta) {
    let default_meta = PatchMeta {
        origin_wx: world_x,
        origin_wy: world_y,
        px_per_unit: target_px_per_unit,
    };
    let Some(view) = floor_overlay else {
        return (None, default_meta);
    };
    let Ok(gray) = frame_view_to_gray_alpha_zeroed(view) else {
        return (None, default_meta);
    };
    extract_underground_patch_gray(
        gray.view(),
        bounds,
        world_x,
        world_y,
        half_extent_world,
        target_px_per_unit,
        reflect_y,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn extract_underground_patch_from_source(
    source: &dyn UndergroundOverlaySource,
    map_id: i32,
    floor_key: &str,
    bounds: &OverlayBounds,
    world_x: f64,
    world_y: f64,
    half_extent_world: f64,
    target_px_per_unit: f64,
    async_fetch: bool,
    reflect_y: Option<bool>,
) -> (Option<Array2<u8>>, PatchMeta) {
    let default_meta = PatchMeta {
        origin_wx: world_x,
        origin_wy: world_y,
        px_per_unit: target_px_per_unit,
    };
    let Some(bytes) = source.get_overlay_bytes(map_id, floor_key, async_fetch) else {
        return (None, default_meta);
    };
    let Ok(gray) = overlay_bytes_to_gray(&bytes) else {
        return (None, default_meta);
    };
    extract_underground_patch_gray(
        gray.view(),
        bounds,
        world_x,
        world_y,
        half_extent_world,
        target_px_per_unit,
        reflect_y,
    )
}

struct LruNode {
    key: TileKey,
    tile: Arc<Array2<u8>>,
    prev: Option<usize>,
    next: Option<usize>,
}

struct LruState {
    capacity: usize,
    map: HashMap<TileKey, usize>,
    nodes: Vec<Option<LruNode>>,
    free: Vec<usize>,
    head: Option<usize>, // LRU
    tail: Option<usize>, // MRU
    ram_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl LruState {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: HashMap::with_capacity(capacity.min(256)),
            nodes: Vec::with_capacity(capacity.min(256)),
            free: Vec::new(),
            head: None,
            tail: None,
            ram_bytes: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    fn detach(&mut self, idx: usize) {
        let (prev, next) = {
            let n = self.nodes[idx].as_ref().expect("valid node");
            (n.prev, n.next)
        };
        match prev {
            Some(p) => self.nodes[p].as_mut().expect("prev").next = next,
            None => self.head = next,
        }
        match next {
            Some(nx) => self.nodes[nx].as_mut().expect("next").prev = prev,
            None => self.tail = prev,
        }
        let n = self.nodes[idx].as_mut().expect("node");
        n.prev = None;
        n.next = None;
    }

    fn push_tail(&mut self, idx: usize) {
        let old_tail = self.tail;
        {
            let n = self.nodes[idx].as_mut().expect("node");
            n.prev = old_tail;
            n.next = None;
        }
        match old_tail {
            Some(t) => self.nodes[t].as_mut().expect("tail").next = Some(idx),
            None => self.head = Some(idx),
        }
        self.tail = Some(idx);
    }

    fn get_hit(&mut self, key: &TileKey) -> Option<Arc<Array2<u8>>> {
        let Some(&idx) = self.map.get(key) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        if self.tail != Some(idx) {
            self.detach(idx);
            self.push_tail(idx);
        }
        self.nodes[idx].as_ref().map(|n| Arc::clone(&n.tile))
    }

    fn contains(&self, key: &TileKey) -> bool {
        self.map.contains_key(key)
    }

    fn put(&mut self, key: TileKey, tile: Arc<Array2<u8>>) {
        let new_bytes = tile.len();
        if let Some(&idx) = self.map.get(&key) {
            let old_bytes = self.nodes[idx].as_ref().map_or(0, |n| n.tile.len());
            self.ram_bytes = self.ram_bytes - old_bytes + new_bytes;
            self.nodes[idx].as_mut().expect("node").tile = tile;
            if self.tail != Some(idx) {
                self.detach(idx);
                self.push_tail(idx);
            }
            return;
        }

        let idx = if let Some(free_idx) = self.free.pop() {
            self.nodes[free_idx] = Some(LruNode {
                key,
                tile,
                prev: None,
                next: None,
            });
            free_idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Some(LruNode {
                key,
                tile,
                prev: None,
                next: None,
            }));
            idx
        };
        self.map.insert(key, idx);
        self.ram_bytes += new_bytes;
        self.push_tail(idx);

        while self.map.len() > self.capacity {
            let Some(evict_idx) = self.head else {
                break;
            };
            self.detach(evict_idx);
            if let Some(evicted) = self.nodes[evict_idx].take() {
                self.map.remove(&evicted.key);
                self.ram_bytes = self.ram_bytes.saturating_sub(evicted.tile.len());
                self.free.push(evict_idx);
                self.evictions += 1;
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.nodes.clear();
        self.free.clear();
        self.head = None;
        self.tail = None;
        self.ram_bytes = 0;
    }
}

#[inline]
fn is_encoded_image_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"RIFF")
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

fn parse_meta_array(meta: &[f64; 5]) -> TileMapMeta {
    TileMapMeta::from(*meta)
}

fn parse_meta_from_npy(arr: &gh_core::npz::NpyArray) -> CvResult<TileMapMeta> {
    let vals: Vec<f64> = match &arr.data {
        NpyData::I64(v) => v.iter().map(|&x| x as f64).collect(),
        NpyData::I32(v) => v.iter().map(|&x| x as f64).collect(),
        NpyData::F32(v) => v.iter().map(|&x| x as f64).collect(),
        NpyData::U8(v) => v.iter().map(|&x| x as f64).collect(),
    };
    if vals.len() < 5 {
        return Err(CvError::ShapeMismatch(format!(
            "terrain meta expected >=5 elements, got {}",
            vals.len()
        )));
    }
    Ok(TileMapMeta {
        tx_start: vals[0].round_ties_even() as i32,
        ty_start: vals[1].round_ties_even() as i32,
        native_z: vals[2].round_ties_even() as i32,
        orig_x: vals[3],
        orig_y: vals[4],
    })
}

pub struct LruTerrainTileCache {
    max_tiles: usize,
    tile_size: usize,
    disk_cache_dir: PathBuf,
    npz_path: Option<PathBuf>,
    tile_provider: Option<Arc<dyn TileProvider>>,
    async_fetch: bool,
    external_terrain: BTreeMap<i32, Arc<TerrainRaster>>,
    lazy_npz_slot: Mutex<Option<(i32, Arc<TerrainRaster>)>>,
    map_meta: Mutex<HashMap<i32, TileMapMeta>>,
    lru: Mutex<LruState>,
    disk_loads: AtomicU64,
    provider_loads: AtomicU64,
}

impl LruTerrainTileCache {
    pub fn from_config(cfg: TileCacheConfig) -> CvResult<Self> {
        if cfg.max_tiles < 1 {
            return Err(CvError::InvalidArgument(format!(
                "max_tiles must be >= 1, got {}",
                cfg.max_tiles
            )));
        }
        if cfg.tile_size < MIN_TILE_SIZE {
            return Err(CvError::InvalidArgument(format!(
                "tile_size must be >= {MIN_TILE_SIZE}, got {}",
                cfg.tile_size
            )));
        }
        let disk_cache_dir = cfg.disk_cache_dir.unwrap_or_else(default_disk_tile_dir);
        let npz_path = match cfg.npz_path {
            Some(p) => {
                if p.is_file() {
                    Some(p)
                } else {
                    None
                }
            }
            None if cfg.use_default_npz => {
                let p = resolve_data_asset("terrain_n2_v4.npz");
                if p.is_file() { Some(p) } else { None }
            }
            None => None,
        };

        let mut initial_meta = HashMap::with_capacity(cfg.terrain_data.len());
        for (&mid, raster) in &cfg.terrain_data {
            initial_meta.insert(mid, parse_meta_array(&raster.meta));
        }

        Ok(Self {
            max_tiles: cfg.max_tiles,
            tile_size: cfg.tile_size,
            disk_cache_dir,
            npz_path,
            tile_provider: cfg.tile_provider,
            async_fetch: cfg.async_fetch,
            external_terrain: cfg.terrain_data,
            lazy_npz_slot: Mutex::new(None),
            map_meta: Mutex::new(initial_meta),
            lru: Mutex::new(LruState::new(cfg.max_tiles)),
            disk_loads: AtomicU64::new(0),
            provider_loads: AtomicU64::new(0),
        })
    }

    pub fn new(max_tiles: usize, tile_size: usize) -> CvResult<Self> {
        Self::from_config(TileCacheConfig {
            max_tiles,
            tile_size,
            ..TileCacheConfig::default()
        })
    }

    pub fn max_tiles(&self) -> usize {
        self.max_tiles
    }

    pub fn tile_size(&self) -> usize {
        self.tile_size
    }

    pub fn disk_cache_dir(&self) -> &Path {
        &self.disk_cache_dir
    }

    pub fn npz_path(&self) -> Option<&Path> {
        self.npz_path.as_deref()
    }

    pub fn tile_provider(&self) -> Option<&Arc<dyn TileProvider>> {
        self.tile_provider.as_ref()
    }

    pub fn tile_count(&self) -> usize {
        self.lru.lock().expect("lru lock").map.len()
    }

    pub fn approx_ram_bytes(&self) -> usize {
        self.lru.lock().expect("lru lock").ram_bytes
    }

    pub fn hits(&self) -> u64 {
        self.lru.lock().expect("lru lock").hits
    }

    pub fn misses(&self) -> u64 {
        self.lru.lock().expect("lru lock").misses
    }

    pub fn evictions(&self) -> u64 {
        self.lru.lock().expect("lru lock").evictions
    }

    pub fn disk_loads(&self) -> u64 {
        self.disk_loads.load(Ordering::Acquire)
    }

    pub fn provider_loads(&self) -> u64 {
        self.provider_loads.load(Ordering::Acquire)
    }

    #[inline]
    pub fn provider_load_seq(&self) -> u64 {
        let local = self.provider_loads.load(Ordering::Acquire);
        let ext = self
            .tile_provider
            .as_ref()
            .map_or(0, |p| p.downloads());
        local.saturating_add(ext)
    }

    pub fn stats(&self) -> CacheStats {
        let g = self.lru.lock().expect("lru lock");
        CacheStats {
            tile_count: g.map.len(),
            approx_ram_bytes: g.ram_bytes,
            hits: g.hits,
            misses: g.misses,
            evictions: g.evictions,
            disk_loads: self.disk_loads.load(Ordering::Acquire),
            provider_loads: self.provider_loads.load(Ordering::Acquire),
        }
    }

    pub fn clear(&self) {
        self.lru.lock().expect("lru lock").clear();
    }

    pub fn register_map_meta(
        &self,
        map_id: i32,
        orig_x: f64,
        orig_y: f64,
        native_zoom: i32,
        tx_start: i32,
        ty_start: i32,
    ) {
        let mut guard = self.map_meta.lock().expect("map_meta lock");
        guard.insert(
            map_id,
            TileMapMeta {
                tx_start,
                ty_start,
                native_z: native_zoom,
                orig_x,
                orig_y,
            },
        );
    }

    fn disk_tile_path(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> PathBuf {
        self.disk_cache_dir
            .join(map_id.to_string())
            .join(zoom.to_string())
            .join(format!("{tx}_{ty}.png"))
    }

    fn normalize_tile(&self, gray: Array2<u8>) -> Option<Array2<u8>> {
        let (h, w) = gray.dim();
        if h == 0 || w == 0 {
            return None;
        }
        if h == self.tile_size && w == self.tile_size {
            return if gray.is_standard_layout() {
                Some(gray)
            } else {
                Some(gray.as_standard_layout().into_owned())
            };
        }
        let interp = if h.max(w) > self.tile_size {
            Interpolation::Area
        } else {
            Interpolation::Linear
        };
        resize_u8(gray.view(), self.tile_size, self.tile_size, interp).ok()
    }

    fn get_npz_source(&self, map_id: i32) -> Option<(Arc<TerrainRaster>, TileMapMeta)> {
        if let Some(raster) = self.external_terrain.get(&map_id) {
            let meta = parse_meta_array(&raster.meta);
            let mut mm = self.map_meta.lock().expect("map_meta lock");
            mm.entry(map_id).or_insert(meta);
            return Some((Arc::clone(raster), meta));
        }

        if let Some((cached_mid, raster)) = self
            .lazy_npz_slot
            .lock()
            .expect("lazy_npz_slot lock")
            .as_ref()
            .filter(|(mid, _)| *mid == map_id)
            .map(|(mid, r)| (*mid, Arc::clone(r)))
        {
            let _ = cached_mid;
            let meta = parse_meta_array(&raster.meta);
            return Some((raster, meta));
        }

        let npz_path = self.npz_path.as_ref()?;
        if !npz_path.is_file() {
            return None;
        }
        let gray_key = format!("map_{map_id}_gray");
        let meta_key = format!("map_{map_id}_meta");
        let gray_arr = NpzArchive::read_single_from_file(npz_path, &gray_key).ok()?;
        let meta_arr = NpzArchive::read_single_from_file(npz_path, &meta_key).ok()?;
        let parsed_meta = parse_meta_from_npy(&meta_arr).ok()?;
        if gray_arr.shape.len() != 2 {
            return None;
        }
        let h = gray_arr.shape[0];
        let w = gray_arr.shape[1];
        let NpyData::U8(raw_bytes) = gray_arr.data else {
            return None;
        };
        let gray = Array2::from_shape_vec((h, w), raw_bytes).ok()?;
        let raster = Arc::new(TerrainRaster {
            gray,
            meta: [
                parsed_meta.tx_start as f64,
                parsed_meta.ty_start as f64,
                parsed_meta.native_z as f64,
                parsed_meta.orig_x,
                parsed_meta.orig_y,
            ],
        });
        {
            let mut lazy = self.lazy_npz_slot.lock().expect("lazy_npz_slot lock");
            *lazy = Some((map_id, Arc::clone(&raster)));
        }
        {
            let mut mm = self.map_meta.lock().expect("map_meta lock");
            mm.insert(map_id, parsed_meta);
        }
        Some((raster, parsed_meta))
    }

    fn get_map_meta(&self, map_id: i32) -> Option<TileMapMeta> {
        if let Some(&m) = self.map_meta.lock().expect("map_meta lock").get(&map_id) {
            return Some(m);
        }
        self.get_npz_source(map_id).map(|(_, m)| m)
    }

    fn load_tile_uncached(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> Option<Array2<u8>> {
        // 1. Local disk cache
        let disk_path = self.disk_tile_path(map_id, zoom, tx, ty);
        if disk_path.is_file()
            && let Ok(decoded) = read_image(&disk_path, ImageReadMode::Unchanged)
            && let Ok(gray) = decoded_image_to_gray_alpha_zeroed(&decoded)
            && let Some(norm) = self.normalize_tile(gray)
        {
            self.disk_loads.fetch_add(1, Ordering::Release);
            return Some(norm);
        }

        // 2. Optional higher-res TileProvider (raw 256x256 Gray8 or encoded PNG/WebP)
        if let Some(provider) = &self.tile_provider
            && let Some(bytes) = provider.request_tile(map_id, zoom, tx, ty, self.async_fetch)
        {
            let ts = self.tile_size;
            let gray_opt = if bytes.len() == ts * ts && !is_encoded_image_magic(&bytes) {
                Array2::from_shape_vec((ts, ts), bytes).ok()
            } else if bytes.len() == DEFAULT_TILE_SIZE * DEFAULT_TILE_SIZE
                && !is_encoded_image_magic(&bytes)
            {
                Array2::from_shape_vec((DEFAULT_TILE_SIZE, DEFAULT_TILE_SIZE), bytes).ok()
            } else {
                decode_image_bytes(&bytes, ImageReadMode::Unchanged)
                    .ok()
                    .and_then(|d| decoded_image_to_gray_alpha_zeroed(&d).ok())
            };
            if let Some(gray) = gray_opt
                && let Some(norm) = self.normalize_tile(gray)
            {
                self.provider_loads.fetch_add(1, Ordering::Release);
                return Some(norm);
            }
        }

        // 3. Lazy NPZ slice (zero whole-world duplication)
        let (raster, meta) = self.get_npz_source(map_id)?;
        if zoom != meta.native_z {
            return None;
        }
        let ts = self.tile_size as isize;
        let cx0 = (tx as isize - meta.tx_start as isize) * ts;
        let cy0 = (ty as isize - meta.ty_start as isize) * ts;
        let (ch, cw) = raster.gray.dim();
        if cx0 < 0 || cy0 < 0 || cx0 >= cw as isize || cy0 >= ch as isize {
            return None;
        }
        let x0 = cx0 as usize;
        let y0 = cy0 as usize;
        let x1 = cw.min(x0 + self.tile_size);
        let y1 = ch.min(y0 + self.tile_size);
        let sub = raster.gray.slice(s![y0..y1, x0..x1]);
        let (sh, sw) = sub.dim();
        if sh == self.tile_size && sw == self.tile_size {
            return Some(sub.to_owned());
        }
        let mut padded = Array2::<u8>::zeros((self.tile_size, self.tile_size));
        padded.slice_mut(s![0..sh, 0..sw]).assign(&sub);
        Some(padded)
    }

    pub fn get_tile(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> Option<Arc<Array2<u8>>> {
        let key: TileKey = (map_id, zoom, tx, ty);
        if let Some(hit) = self.lru.lock().expect("lru lock").get_hit(&key) {
            return Some(hit);
        }
        let tile = Arc::new(self.load_tile_uncached(map_id, zoom, tx, ty)?);
        self.lru.lock().expect("lru lock").put(key, Arc::clone(&tile));
        Some(tile)
    }

    fn choose_zoom(
        &self,
        map_id: i32,
        world_x: f64,
        world_y: f64,
        target_px_per_unit: f64,
        map_meta: &TileMapMeta,
    ) -> i32 {
        let z_ideal = (target_px_per_unit.max(1e-4).log2().round_ties_even() as i32).clamp(-2, 0);
        if z_ideal == map_meta.native_z {
            return map_meta.native_z;
        }
        let step = if z_ideal > map_meta.native_z { -1 } else { 1 };
        let mut z_cand = z_ideal;
        while z_cand != map_meta.native_z {
            let s_cand = 2.0f64.powi(z_cand);
            let ts_f = self.tile_size as f64;
            let tx_c = ((world_x + map_meta.orig_x) * s_cand / ts_f).floor() as i32;
            let ty_c = ((world_y + map_meta.orig_y) * s_cand / ts_f).floor() as i32;
            let key: TileKey = (map_id, z_cand, tx_c, ty_c);
            if self.lru.lock().expect("lru lock").contains(&key) {
                return z_cand;
            }
            if self.disk_tile_path(map_id, z_cand, tx_c, ty_c).is_file() {
                return z_cand;
            }
            if self.tile_provider.is_some() && self.get_tile(map_id, z_cand, tx_c, ty_c).is_some() {
                return z_cand;
            }
            z_cand += step;
        }
        map_meta.native_z
    }

    pub fn extract_local_patch(
        &self,
        map_id: i32,
        world_x: f64,
        world_y: f64,
        half_extent_world: f64,
        target_px_per_unit: f64,
    ) -> (Option<Array2<u8>>, PatchMeta) {
        let (patch, meta, _) = self.extract_local_patch_with_zoom(
            map_id,
            world_x,
            world_y,
            half_extent_world,
            target_px_per_unit,
        );
        (patch, meta)
    }

    pub fn extract_local_patch_with_zoom(
        &self,
        map_id: i32,
        world_x: f64,
        world_y: f64,
        half_extent_world: f64,
        target_px_per_unit: f64,
    ) -> (Option<Array2<u8>>, PatchMeta, Option<i32>) {
        let default_meta = PatchMeta {
            origin_wx: world_x,
            origin_wy: world_y,
            px_per_unit: target_px_per_unit,
        };
        if half_extent_world <= 0.0
            || target_px_per_unit <= 0.0
            || !world_x.is_finite()
            || !world_y.is_finite()
            || !half_extent_world.is_finite()
            || !target_px_per_unit.is_finite()
        {
            return (None, default_meta, None);
        }

        let raw_out_f = 2.0 * half_extent_world * target_px_per_unit;
        let out_size_f = if raw_out_f.is_finite() {
            raw_out_f.round_ties_even().max(MIN_PATCH_DIM as f64)
        } else {
            f64::INFINITY
        };
        let eff_half = (0.5 * out_size_f) / target_px_per_unit;
        let origin_wx = world_x - eff_half;
        let origin_wy = world_y - eff_half;
        let meta_out = PatchMeta {
            origin_wx,
            origin_wy,
            px_per_unit: target_px_per_unit,
        };

        let Some(map_meta) = self.get_map_meta(map_id) else {
            return (None, meta_out, None);
        };
        let z_src = self.choose_zoom(map_id, world_x, world_y, target_px_per_unit, &map_meta);
        if !out_size_f.is_finite()
            || !eff_half.is_finite()
            || out_size_f > MAX_PATCH_DIM as f64
            || !origin_wx.is_finite()
            || !origin_wy.is_finite()
        {
            return (None, meta_out, Some(z_src));
        }
        let out_size = out_size_f as usize;

        let s_src = 2.0f64.powi(z_src);
        let ts = self.tile_size;
        let ts_f = ts as f64;

        let gx_min = (origin_wx + map_meta.orig_x) * s_src - 1.0;
        let gx_max = (origin_wx + 2.0 * eff_half + map_meta.orig_x) * s_src + 1.0;
        let gy_min = (origin_wy + map_meta.orig_y) * s_src - 1.0;
        let gy_max = (origin_wy + 2.0 * eff_half + map_meta.orig_y) * s_src + 1.0;

        let tx_min_f = (gx_min / ts_f).floor();
        let tx_max_f = (gx_max / ts_f).floor();
        let ty_min_f = (gy_min / ts_f).floor();
        let ty_max_f = (gy_max / ts_f).floor();

        let i32_min_f = i32::MIN as f64;
        let i32_max_f = i32::MAX as f64;
        if !tx_min_f.is_finite()
            || !tx_max_f.is_finite()
            || !ty_min_f.is_finite()
            || !ty_max_f.is_finite()
            || tx_min_f < i32_min_f
            || tx_max_f > i32_max_f
            || ty_min_f < i32_min_f
            || ty_max_f > i32_max_f
        {
            return (None, meta_out, Some(z_src));
        }

        let tx_min_i64 = tx_min_f as i64;
        let tx_max_i64 = tx_max_f as i64;
        let ty_min_i64 = ty_min_f as i64;
        let ty_max_i64 = ty_max_f as i64;

        let cols = tx_max_i64 - tx_min_i64 + 1;
        let rows = ty_max_i64 - ty_min_i64 + 1;
        let max_mosaic = MAX_MOSAIC_TILES as i64;
        if cols <= 0
            || rows <= 0
            || cols > max_mosaic
            || rows > max_mosaic
            || cols.checked_mul(rows).is_none_or(|p| p > max_mosaic)
        {
            return (None, meta_out, Some(z_src));
        }

        let tx_min = tx_min_i64 as i32;
        let tx_max = tx_max_i64 as i32;
        let ty_min = ty_min_i64 as i32;
        let ty_max = ty_max_i64 as i32;

        let mut mosaic = Array2::<u8>::zeros((rows as usize * ts, cols as usize * ts));
        let mut loaded_any = false;
        for ty in ty_min..=ty_max {
            let ry = (ty - ty_min) as usize * ts;
            for tx in tx_min..=tx_max {
                if let Some(tile) = self.get_tile(map_id, z_src, tx, ty) {
                    let rx = (tx - tx_min) as usize * ts;
                    mosaic
                        .slice_mut(s![ry..ry + ts, rx..rx + ts])
                        .assign(tile.as_ref());
                    loaded_any = true;
                }
            }
        }
        if !loaded_any {
            return (None, meta_out, Some(z_src));
        }

        let scale_ratio = s_src / target_px_per_unit;
        let off_x = (origin_wx + map_meta.orig_x) * s_src - (tx_min as f64) * ts_f
            + 0.5 * (scale_ratio - 1.0);
        let off_y = (origin_wy + map_meta.orig_y) * s_src - (ty_min as f64) * ts_f
            + 0.5 * (scale_ratio - 1.0);

        let m = [scale_ratio, 0.0, off_x, 0.0, scale_ratio, off_y];
        if !scale_ratio.is_finite()
            || !off_x.is_finite()
            || !off_y.is_finite()
            || !affine_corners_safe(&m, out_size)
        {
            return (None, meta_out, Some(z_src));
        }
        let interp = if scale_ratio > SCALE_AREA_THRESHOLD {
            Interpolation::Area
        } else {
            Interpolation::Linear
        };
        let patch = warp_affine_u8(
            mosaic.view(),
            &m,
            out_size,
            out_size,
            interp,
            true,
            BorderMode::Constant(0.0),
        )
        .ok();
        (patch, meta_out, Some(z_src))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn extract_underground_patch(
        &self,
        floor_overlay: Option<&DecodedImage>,
        bounds: &OverlayBounds,
        world_x: f64,
        world_y: f64,
        half_extent_world: f64,
        target_px_per_unit: f64,
        reflect_y: Option<bool>,
    ) -> (Option<Array2<u8>>, PatchMeta) {
        extract_underground_patch(
            floor_overlay,
            bounds,
            world_x,
            world_y,
            half_extent_world,
            target_px_per_unit,
            reflect_y,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_io::write_image_gray;
    use crate::ops::ncc::{match_template_f32, min_max_loc_f32};
    use crate::ops::TemplateMatchMode;
    use gh_core::npz::NpyArray;
    use image::{ImageBuffer, Rgba};
    use std::io::Cursor;

    struct TempDirGuard(PathBuf);
    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "{prefix}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct MockTileProvider {
        raw_mode: bool,
        value: u8,
        size: usize,
    }

    impl TileProvider for MockTileProvider {
        fn request_tile(
            &self,
            _map_id: i32,
            z: i32,
            tx: i32,
            ty: i32,
            _async_fetch: bool,
        ) -> Option<Vec<u8>> {
            if z != -1 || tx != 10 || ty != 12 {
                return None;
            }
            if self.raw_mode {
                Some(vec![self.value; self.size * self.size])
            } else {
                let img = image::GrayImage::from_pixel(
                    self.size as u32,
                    self.size as u32,
                    image::Luma([self.value]),
                );
                let mut buf = Vec::new();
                img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                    .unwrap();
                Some(buf)
            }
        }
    }

    struct MockUgSource {
        png_bytes: Vec<u8>,
    }

    impl UndergroundOverlaySource for MockUgSource {
        fn get_overlay_bytes(&self, map_id: i32, floor_id: &str, _async_fetch: bool) -> Option<Vec<u8>> {
            if map_id == 2 && (floor_id == "501" || floor_id == "100:501") {
                Some(self.png_bytes.clone())
            } else {
                None
            }
        }
    }

    #[test]
    fn lru_eviction_and_ram_bound() {
        let mut terrain = BTreeMap::new();
        let canvas = Array2::<u8>::from_elem((256 * 6, 256 * 6), 120);
        terrain.insert(
            2,
            Arc::new(TerrainRaster {
                gray: canvas,
                meta: [0.0, 0.0, -2.0, 0.0, 0.0],
            }),
        );
        let tmp = TempDirGuard::new("gh_tc_lru");
        let cache = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.clone()),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap();

        for ty in 0..5 {
            for tx in 0..6 {
                let t = cache.get_tile(2, -2, tx, ty).unwrap();
                assert_eq!(t.dim(), (256, 256));
                assert!(cache.tile_count() <= 16);
            }
        }
        assert_eq!(cache.tile_count(), 16);
        assert_eq!(cache.evictions(), 14);
        assert_eq!(cache.approx_ram_bytes(), 16 * 256 * 256);

        // Touch oldest currently cached key (tx=2, ty=2 -> 14th inserted) to promote to MRU
        let hot_key = (2, -2, 2, 2);
        assert!(cache.get_tile(hot_key.0, hot_key.1, hot_key.2, hot_key.3).is_some());
        // Insert one more tile -> should NOT evict hot_key
        assert!(cache.get_tile(2, -2, 5, 5).is_some());
        let hits_before = cache.hits();
        assert!(cache.get_tile(hot_key.0, hot_key.1, hot_key.2, hot_key.3).is_some());
        assert_eq!(cache.hits(), hits_before + 1);
    }

    #[test]
    fn disk_precedence_and_provider_raw_vs_png() {
        let tmp = TempDirGuard::new("gh_tc_prov");
        let unicode_disk = tmp.0.join("tiếng_việt_cache");
        let disk_tile_path = unicode_disk.join("2").join("-1").join("10_12.png");
        let disk_arr = Array2::<u8>::from_elem((256, 256), 173);
        write_image_gray(&disk_tile_path, disk_arr.view(), None).unwrap();

        // Disk takes precedence over raw provider
        let raw_prov = Arc::new(MockTileProvider {
            raw_mode: true,
            value: 99,
            size: 256,
        });
        let cache = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(unicode_disk.clone()),
            terrain_data: BTreeMap::new(),
            tile_provider: Some(raw_prov.clone()),
            async_fetch: false,
        })
        .unwrap();
        let got_disk = cache.get_tile(2, -1, 10, 12).unwrap();
        assert_eq!(got_disk[[10, 10]], 173);
        assert_eq!(cache.provider_load_seq(), 0);

        // Empty disk dir -> hits raw 256x256 provider
        let tmp2 = TempDirGuard::new("gh_tc_prov_raw");
        let cache_raw = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp2.0.clone()),
            terrain_data: BTreeMap::new(),
            tile_provider: Some(raw_prov),
            async_fetch: false,
        })
        .unwrap();
        let got_raw = cache_raw.get_tile(2, -1, 10, 12).unwrap();
        assert_eq!(got_raw[[5, 5]], 99);
        assert_eq!(cache_raw.provider_load_seq(), 1);

        // Non-256 PNG provider resized cleanly to 256x256
        let png_prov = Arc::new(MockTileProvider {
            raw_mode: false,
            value: 211,
            size: 128,
        });
        let cache_png = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp2.0.clone()),
            terrain_data: BTreeMap::new(),
            tile_provider: Some(png_prov),
            async_fetch: false,
        })
        .unwrap();
        let got_png = cache_png.get_tile(2, -1, 10, 12).unwrap();
        assert_eq!(got_png.dim(), (256, 256));
        assert_eq!(got_png[[64, 64]], 211);
        assert_eq!(cache_png.provider_load_seq(), 1);
    }

    #[test]
    fn npz_lazy_slicing_and_edge_padding() {
        let tmp = TempDirGuard::new("gh_tc_npz");
        let npz_path = tmp.0.join("synth_terrain.npz");
        let mut canvas = Array2::<u8>::zeros((300, 300));
        for y in 0..300 {
            for x in 0..300 {
                canvas[[y, x]] = ((x * 3 + y * 5) % 240 + 10) as u8;
            }
        }
        let mut archive = NpzArchive::new();
        archive.insert(
            "map_2_gray",
            NpyArray::from_u8(vec![300, 300], canvas.iter().copied().collect()).unwrap(),
        );
        archive.insert(
            "map_2_meta",
            NpyArray::from_i64(vec![5], vec![7, 6, -2, 2000, 2000]).unwrap(),
        );
        archive.write_file_atomic(&npz_path).unwrap();

        let cache = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: Some(npz_path),
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: BTreeMap::new(),
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap();

        assert!(cache.get_tile(2, -1, 7, 6).is_none());
        let edge_tile = cache.get_tile(2, -2, 8, 7).unwrap();
        assert_eq!(edge_tile.dim(), (256, 256));
        assert_eq!(edge_tile[[43, 43]], canvas[[299, 299]]);
        assert_eq!(edge_tile[[44, 44]], 0);
        assert_eq!(edge_tile[[200, 200]], 0);
    }

    #[test]
    fn extract_local_patch_subpixel_and_mosaic_cap() {
        let mut canvas = Array2::<u8>::zeros((512, 512));
        for y in 0..512 {
            for x in 0..512 {
                canvas[[y, x]] = (((x * 13 + y * 7) % 180) + 30) as u8;
            }
        }
        let mut terrain = BTreeMap::new();
        terrain.insert(
            2,
            Arc::new(TerrainRaster {
                gray: canvas.clone(),
                meta: [7.0, 6.0, -2.0, 2000.0, 2000.0],
            }),
        );
        let tmp = TempDirGuard::new("gh_tc_patch");
        let cache = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.clone()),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap();

        // Center of 512x512 canvas in world coords: px=256.0 -> wx = (7*256 + 256)/0.25 - 2000 = 6192
        let wx = 6192.0;
        let wy = 5168.0;
        let (patch_opt, meta, zoom) = cache.extract_local_patch_with_zoom(2, wx, wy, 200.0, 0.25);
        assert_eq!(zoom, Some(-2));
        let patch = patch_opt.unwrap();
        assert_eq!(patch.dim(), (100, 100));
        let (cx, cy) = meta.pixel_to_world(49.5, 49.5);
        assert!((cx - wx).abs() < 1e-6 && (cy - wy).abs() < 1e-6);

        let ref_crop = canvas.slice(s![206..306, 206..306]).mapv(|v| v as f32);
        let patch_f = patch.mapv(|v| v as f32);
        let ncc = match_template_f32(ref_crop.view(), patch_f.view(), TemplateMatchMode::CcoeffNormed)
            .unwrap();
        let (_, max_v, _, _) = min_max_loc_f32(ncc.view()).unwrap();
        assert!(max_v > 0.999, "expected NCC > 0.999, got {max_v}");

        // Reject excessive mosaic span (> 64 tiles)
        let (too_big, _, _) = cache.extract_local_patch_with_zoom(2, wx, wy, 50_000.0, 0.25);
        assert!(too_big.is_none());
    }

    #[test]
    fn underground_y_reflect_cases_4a_4b_4c_and_alpha_zeroing() {
        let mut ug_north_up = Array2::<u8>::from_elem((200, 200), 40);
        ug_north_up.slice_mut(s![20..55, 70..130]).fill(235);
        ug_north_up.slice_mut(s![150..180, 70..130]).fill(95);

        // Case 4a: standard l_y < r_y
        let bounds_std = OverlayBounds::new(-4000.0, -7400.0, -3200.0, -6600.0);
        let (p_std_opt, _) =
            extract_underground_patch_gray(ug_north_up.view(), &bounds_std, -3600.0, -7000.0, 400.0, 0.25, None);
        let p_std = p_std_opt.unwrap();
        let top_mean = p_std.slice(s![0..50, ..]).iter().map(|&v| v as f64).sum::<f64>() / (50.0 * 200.0);
        let bot_mean = p_std.slice(s![50..200, ..]).iter().map(|&v| v as f64).sum::<f64>() / (150.0 * 200.0);
        assert!(top_mean > bot_mean + 8.0, "top={top_mean}, bot={bot_mean}");

        // Case 4b: opposite-sign HoYoLAB bounds
        let bounds_opp = OverlayBounds::new(-4000.0, 7400.0, -3200.0, 6600.0);
        let (mat_opp, applied_reflect) = compose_underground_local_to_surface(
            &bounds_opp,
            200,
            200,
            Some(-7000.0),
            400.0,
            None,
        )
        .unwrap();
        assert!(applied_reflect);
        assert!(mat_opp[1][1] > 0.0);
        let (p_opp_opt, _) =
            extract_underground_patch_gray(ug_north_up.view(), &bounds_opp, -3600.0, -7000.0, 400.0, 0.25, None);
        let p_opp = p_opp_opt.unwrap();
        let diff_opp: f64 = p_std
            .iter()
            .zip(p_opp.iter())
            .map(|(&a, &b)| (a as i32 - b as i32).abs() as f64)
            .sum::<f64>()
            / p_std.len() as f64;
        assert!(diff_opp < 1.0, "diff_opp={diff_opp}");

        // Case 4c: south-up upside-down overlay (l_y > r_y)
        let mut ug_south_up = Array2::<u8>::zeros((200, 200));
        for y in 0..200 {
            ug_south_up.row_mut(y).assign(&ug_north_up.row(199 - y));
        }
        let bounds_flip = OverlayBounds::new(-4000.0, -6600.0, -3200.0, -7400.0);
        let (p_flip_opt, _) =
            extract_underground_patch_gray(ug_south_up.view(), &bounds_flip, -3600.0, -7000.0, 400.0, 0.25, None);
        let p_flip = p_flip_opt.unwrap();
        let diff_flip: f64 = p_std
            .iter()
            .zip(p_flip.iter())
            .map(|(&a, &b)| (a as i32 - b as i32).abs() as f64)
            .sum::<f64>()
            / p_std.len() as f64;
        assert!(diff_flip < 1.5, "diff_flip={diff_flip}");

        // Non-overlapping query rejected
        let (oob, _) =
            extract_underground_patch_gray(ug_north_up.view(), &bounds_std, 1000.0, 1000.0, 200.0, 0.25, None);
        assert!(oob.is_none());

        // FloorOverlayMeta directional preservation (l_y > r_y)
        let meta_dir = FloorOverlayMeta {
            l_x: -4000.0,
            l_y: -6600.0,
            r_x: -3200.0,
            r_y: -7400.0,
            ..FloorOverlayMeta::default()
        };
        let ob_dir = OverlayBounds::from_floor_overlay_meta(&meta_dir);
        assert_eq!(ob_dir.ly, -6600.0);
        assert_eq!(ob_dir.ry, -7400.0);

        // Alpha < 16 zeroing via PNG + UndergroundOverlaySource
        let rgba = ImageBuffer::from_fn(64, 64, |x, _y| {
            if x < 32 {
                Rgba([240u8, 240, 240, 10]) // transparent < 16 -> must become 0
            } else {
                Rgba([200u8, 200, 200, 255])
            }
        });
        let mut png_bytes = Vec::new();
        rgba.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();
        let gray_zeroed = overlay_bytes_to_gray(&png_bytes).unwrap();
        assert_eq!(gray_zeroed[[10, 10]], 0);
        assert!(gray_zeroed[[10, 48]] > 180);

        let src = MockUgSource { png_bytes };
        let bounds_small = OverlayBounds::new(-100.0, -100.0, 100.0, 100.0);
        let (ug_from_src, _) = extract_underground_patch_from_source(
            &src,
            2,
            "100:501",
            &bounds_small,
            0.0,
            0.0,
            80.0,
            0.5,
            true,
            None,
        );
        let ug_arr = ug_from_src.unwrap();
        assert_eq!(ug_arr.dim(), (80, 80));
        assert_eq!(ug_arr[[40, 10]], 0);
        assert!(ug_arr[[40, 65]] > 150);
    }

    #[test]
    fn extract_patch_extreme_half_extent_and_scale_no_overflow() {
        let mut terrain = BTreeMap::new();
        terrain.insert(
            2,
            Arc::new(TerrainRaster {
                gray: Array2::<u8>::from_elem((256, 256), 128),
                meta: [7.0, 6.0, -2.0, 2000.0, 2000.0],
            }),
        );
        let tmp = TempDirGuard::new("gh_tc_overflow");
        let cache = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.clone()),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap();

        let wx = 6192.0;
        let wy = 5168.0;
        let shared_extreme_pairs = [
            (1e9, 0.25),
            (1e12, 0.25),
            (1e30, 0.25),
            (f64::MAX / 4.0, 0.25),
            (f64::MAX / 4.0, 16.0),
            (1.0, 1e20),
            (100.0, 1e-20),
            (100.0, 1e-29),
            (100.0, 1e-300),
            (100.0, 1e-308),
            (1e9, 1e-20),
            (1e30, 1e-300),
            (f64::MAX / 4.0, 1e-307),
        ];
        for (half, ppu) in shared_extreme_pairs
            .into_iter()
            .chain([(1e9, 1e-12), (1e12, 1e-12)])
        {
            let (p_zoom, _, z_src) = cache.extract_local_patch_with_zoom(2, wx, wy, half, ppu);
            assert!(p_zoom.is_none(), "expected None for half={half}, ppu={ppu}");
            assert!(z_src.is_some(), "expected Some(z_src) for half={half}, ppu={ppu}");

            let (p_local, _) = cache.extract_local_patch(2, wx, wy, half, ppu);
            assert!(p_local.is_none());
        }

        // Extreme world origin with moderate half_extent_world must not saturate i32 tile bounds
        let (p_far, _, z_far) = cache.extract_local_patch_with_zoom(2, 1e30, 1e30, 100.0, 0.25);
        assert!(p_far.is_none());
        assert!(z_far.is_some());

        // Underground patch extractor must also reject extreme half_extent / tiny-scale cleanly
        let ug = Array2::<u8>::from_elem((64, 64), 100);
        let bounds = OverlayBounds::new(-4000.0, -7400.0, -3200.0, -6600.0);
        for reflect in [None, Some(true), Some(false)] {
            for (half, ppu) in shared_extreme_pairs {
                let (p_ug, _) = extract_underground_patch_gray(
                    ug.view(),
                    &bounds,
                    -3600.0,
                    -7000.0,
                    half,
                    ppu,
                    reflect,
                );
                assert!(
                    p_ug.is_none(),
                    "expected None underground patch for half={half}, ppu={ppu}, reflect={reflect:?}"
                );
            }
            for (half, ppu) in [(1e9, 1e-12), (1e12, 1e-12)] {
                let (p_ug, _) = extract_underground_patch_gray(
                    ug.view(),
                    &bounds,
                    -3600.0,
                    -7000.0,
                    half,
                    ppu,
                    reflect,
                );
                if let Some(arr) = p_ug {
                    assert_eq!(arr.dim(), (MIN_PATCH_DIM, MIN_PATCH_DIM));
                }
            }
        }

        // Invalid / boundary scalar inputs (negative, 0.0, -0.0, NaN, +Inf, -Inf) on both extractors
        let bad_scalars = [-1.0, 0.0, -0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for bad in bad_scalars {
            let (p_half, _, z_half) = cache.extract_local_patch_with_zoom(2, wx, wy, bad, 0.25);
            assert!(p_half.is_none(), "surface half={bad:?}");
            assert!(z_half.is_none(), "surface half={bad:?} zoom");

            let (p_ppu, _, z_ppu) = cache.extract_local_patch_with_zoom(2, wx, wy, 100.0, bad);
            assert!(p_ppu.is_none(), "surface ppu={bad:?}");
            assert!(z_ppu.is_none(), "surface ppu={bad:?} zoom");

            let (p_ug_half, _) =
                extract_underground_patch_gray(ug.view(), &bounds, -3600.0, -7000.0, bad, 0.25, None);
            assert!(p_ug_half.is_none(), "ug half={bad:?}");

            let (p_ug_ppu, _) =
                extract_underground_patch_gray(ug.view(), &bounds, -3600.0, -7000.0, 100.0, bad, None);
            assert!(p_ug_ppu.is_none(), "ug ppu={bad:?}");

            // world_x / world_y: non-finite must return None; negative / +-0.0 must not panic
            let (p_wx, _, z_wx) = cache.extract_local_patch_with_zoom(2, bad, wy, 100.0, 0.25);
            let (p_wy, _, z_wy) = cache.extract_local_patch_with_zoom(2, wx, bad, 100.0, 0.25);
            let (p_ug_wx, _) =
                extract_underground_patch_gray(ug.view(), &bounds, bad, -7000.0, 100.0, 0.25, None);
            let (p_ug_wy, _) =
                extract_underground_patch_gray(ug.view(), &bounds, -3600.0, bad, 100.0, 0.25, None);
            if !bad.is_finite() {
                assert!(p_wx.is_none() && z_wx.is_none(), "surface non-finite wx={bad:?}");
                assert!(p_wy.is_none() && z_wy.is_none(), "surface non-finite wy={bad:?}");
                assert!(p_ug_wx.is_none(), "ug non-finite wx={bad:?}");
                assert!(p_ug_wy.is_none(), "ug non-finite wy={bad:?}");
            } else {
                // -1.0, 0.0, -0.0 are outside the fixture raster and bounds -> cleanly None
                assert!(p_wx.is_none(), "surface out-of-raster wx={bad:?}");
                assert!(p_wy.is_none(), "surface out-of-raster wy={bad:?}");
                assert!(p_ug_wx.is_none(), "ug out-of-bounds wx={bad:?}");
                assert!(p_ug_wy.is_none(), "ug out-of-bounds wy={bad:?}");
            }
        }

        let bounds_origin = OverlayBounds::new(-100.0, -100.0, 100.0, 100.0);
        let (p_neg_zero, _) =
            extract_underground_patch_gray(ug.view(), &bounds_origin, -0.0, -0.0, 50.0, 0.5, None);
        assert!(p_neg_zero.is_some());
    }

    #[test]
    fn provider_load_seq_advances_when_async_hd_downloads_complete() {
        use std::sync::atomic::AtomicU64;

        struct AsyncHdUpgradeMock {
            disk_dir: PathBuf,
            downloads: AtomicU64,
        }

        impl TileProvider for AsyncHdUpgradeMock {
            fn request_tile(
                &self,
                _map_id: i32,
                _z: i32,
                _x: i32,
                _y: i32,
                _async_fetch: bool,
            ) -> Option<Vec<u8>> {
                None
            }

            fn downloads(&self) -> u64 {
                self.downloads.load(Ordering::Acquire)
            }
        }

        let tmp = TempDirGuard::new("gh_tc_async_dl");
        let prov = Arc::new(AsyncHdUpgradeMock {
            disk_dir: tmp.0.clone(),
            downloads: AtomicU64::new(0),
        });
        let cache = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.clone()),
            terrain_data: BTreeMap::new(),
            tile_provider: Some(prov.clone()),
            async_fetch: true,
        })
        .unwrap();

        assert!(cache.get_tile(2, -1, 4, 5).is_none());
        assert_eq!(cache.provider_load_seq(), 0);

        // Simulate background worker persisting PNG to disk and bumping downloads()
        let hd_path = prov.disk_dir.join("2").join("-1").join("4_5.png");
        let hd_arr = Array2::<u8>::from_elem((256, 256), 222);
        write_image_gray(&hd_path, hd_arr.view(), None).unwrap();
        prov.downloads.store(1, Ordering::Release);

        // Subsequent lookup resolves via Step 1 disk cache, and provider_load_seq() advances.
        let got = cache.get_tile(2, -1, 4, 5).unwrap();
        assert_eq!(got[[12, 12]], 222);
        assert!(cache.provider_load_seq() >= 1);
    }
}
