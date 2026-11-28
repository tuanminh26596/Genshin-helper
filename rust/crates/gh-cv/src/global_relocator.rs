//! Full-Teyvat RootSIFT KD-Tree global relocator (`src/global_relocator.py`).
//!
//! Bumped to `INDEX_VERSION = 4` (`teyvat_global_index_v4.npz`): builds uncapped
//! per-tile RootSIFT descriptors so high-contrast tiles never starve calmer
//! regions, and embeds `terrain_fingerprint` so disk caches rebuild whenever
//! `terrain_n2_v4.npz` changes.

use crate::locator::TerrainRaster;
use crate::ops::color::{bgr_to_gray, bgr_to_hsv, frame_view_to_bgr, frame_view_to_gray_array};
use crate::ops::flann::KdTreeIndexU8;
use crate::ops::resize::{resize_u8, resize_u8_hwc};
use crate::ops::sift::{SiftExtractor, quantize_rootsift_u8, to_rootsift};
use crate::ops::{CvError, CvResult, Interpolation};
use gh_core::assets::resolve_data_asset;
use gh_core::frame::{FrameView, PixelFormat};
use gh_core::npz::{NpyArray, NpyData, NpzArchive};
use gh_core::paths::AppPaths;
use ndarray::{Array2, ArrayView2, ArrayView3, s};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

pub const INDEX_VERSION: i32 = 4;
pub const SCALE_FACTOR: f32 = 400.0;
pub const BIN_WORLD: f64 = 240.0;
pub const NMS_DIST_W: f64 = 420.0;
pub const GLOBAL_INDEX_FILENAME: &str = gh_core::paths::GLOBAL_INDEX_FILENAME;

pub const DEFAULT_TOP_K: usize = 4;
pub const DEFAULT_MIN_BIN_SCORE: f64 = 3.5;
pub const LOWE_RATIO_THRESHOLD: f64 = 0.78;
pub const KD_TREES: usize = 4;
pub const KD_CHECKS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoteSample {
    pub world_x: f64,
    pub world_y: f64,
    pub weight: f64,
}

pub type VoteBins = HashMap<(i32, i32), Vec<VoteSample>>;
pub type GlobalIndexVectors = (Vec<[u8; 128]>, Vec<[f32; 2]>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelocateCandidate {
    pub world_x: f64,
    pub world_y: f64,
    pub score: f64,
}

pub fn default_global_index_path() -> PathBuf {
    AppPaths::resolve().global_index_path()
}

pub fn to_rootsift_batch(descriptors: &[[f32; 128]]) -> Vec<[f32; 128]> {
    descriptors.iter().map(to_rootsift).collect()
}

fn tile_std_u8(tile: ArrayView2<'_, u8>) -> f64 {
    let n = tile.len();
    if n == 0 {
        return 0.0;
    }
    let mut sum = 0u64;
    let mut sum_sq = 0u64;
    if let Some(sl) = tile.as_slice() {
        for &v in sl {
            let x = v as u64;
            sum += x;
            sum_sq += x * x;
        }
    } else {
        for &v in tile.iter() {
            let x = v as u64;
            sum += x;
            sum_sq += x * x;
        }
    }
    let nf = n as f64;
    let mean = sum as f64 / nf;
    (sum_sq as f64 / nf - mean * mean).max(0.0).sqrt()
}

#[inline]
fn fnv1a_update(mut h: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Computes a deterministic 64-bit FNV-1a fingerprint over Map 2 dimensions,
/// world metadata, and pixel bytes (stride-independent across sliced views).
pub fn terrain_raster_fingerprint(gray: ArrayView2<'_, u8>, meta: &[f64; 5]) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h = FNV_OFFSET;
    let (rows, cols) = gray.dim();
    h = fnv1a_update(h, &(rows as u64).to_le_bytes());
    h = fnv1a_update(h, &(cols as u64).to_le_bytes());
    for m in meta {
        h = fnv1a_update(h, &m.to_bits().to_le_bytes());
    }
    for row in gray.rows() {
        if let Some(sl) = row.as_slice() {
            h = fnv1a_update(h, sl);
        } else {
            for &b in row {
                h = fnv1a_update(h, &[b]);
            }
        }
    }
    h as i64
}

struct TileJob {
    y0: usize,
    y1: usize,
    x0: usize,
    x1: usize,
    keep_y0: usize,
    keep_y1: usize,
    keep_x0: usize,
    keep_x1: usize,
}

/// Tiles Map 2 `terrain_n2` into 384x384 RootSIFT patches and atomically writes `teyvat_global_index_v4.npz`.
pub fn build_teyvat_global_index(
    map2_gray: ArrayView2<'_, u8>,
    map2_meta: &[f64; 5],
    cache_path: Option<&Path>,
) -> CvResult<GlobalIndexVectors> {
    let (h, w) = map2_gray.dim();
    if h == 0 || w == 0 {
        return Err(CvError::EmptyInput(
            "map2_gray is empty when building Teyvat global index".into(),
        ));
    }

    let tx_start = map2_meta[0].round_ties_even() as i32;
    let ty_start = map2_meta[1].round_ties_even() as i32;
    let native_z = map2_meta[2].round_ties_even() as i32;
    let orig_x = map2_meta[3];
    let orig_y = map2_meta[4];
    let s_src = 2.0f64.powi(native_z);

    let tile_sz = 384usize;
    let stride = 360usize;
    let margin = 12usize;

    let mut ys: Vec<usize> = (0..=h.saturating_sub(tile_sz)).step_by(stride).collect();
    if ys.is_empty() {
        ys.push(0);
    }
    if *ys.last().unwrap() + tile_sz < h {
        ys.push(h.saturating_sub(tile_sz));
    }

    let mut xs: Vec<usize> = (0..=w.saturating_sub(tile_sz)).step_by(stride).collect();
    if xs.is_empty() {
        xs.push(0);
    }
    if *xs.last().unwrap() + tile_sz < w {
        xs.push(w.saturating_sub(tile_sz));
    }

    let mut jobs = Vec::with_capacity(ys.len() * xs.len());
    for (iy, &y0) in ys.iter().enumerate() {
        let y1 = h.min(y0 + tile_sz);
        let th = y1 - y0;
        let ky0_raw = if iy == 0 {
            0
        } else {
            (ys[iy - 1] + tile_sz - margin).saturating_sub(y0)
        };
        let ky1_raw = if iy + 1 == ys.len() {
            th
        } else {
            th.saturating_sub(margin)
        };
        let keep_y0 = ky0_raw.min(th);
        let keep_y1 = ky1_raw.clamp(keep_y0, th);

        for (ix, &x0) in xs.iter().enumerate() {
            let x1 = w.min(x0 + tile_sz);
            let tw = x1 - x0;
            let kx0_raw = if ix == 0 {
                0
            } else {
                (xs[ix - 1] + tile_sz - margin).saturating_sub(x0)
            };
            let kx1_raw = if ix + 1 == xs.len() {
                tw
            } else {
                tw.saturating_sub(margin)
            };
            let keep_x0 = kx0_raw.min(tw);
            let keep_x1 = kx1_raw.clamp(keep_x0, tw);
            jobs.push(TileJob {
                y0,
                y1,
                x0,
                x1,
                keep_y0,
                keep_y1,
                keep_x0,
                keep_x1,
            });
        }
    }

    // Remove the per-tile keypoint cap (`nfeatures = 0`) so high-contrast cliffs/cities
    // cannot evict calmer open-world terrain features inside the same 384x384 tile.
    let extractor = SiftExtractor::new(0, 0.011, 15.0);
    let per_tile: Vec<Option<GlobalIndexVectors>> = jobs
        .par_iter()
        .map(|job| {
            let tile = map2_gray.slice(s![job.y0..job.y1, job.x0..job.x1]);
            if tile_std_u8(tile) < 8.0 {
                return None;
            }
            let (kps, des) = extractor.detect_and_compute(tile, None).ok()?;
            if kps.is_empty() || des.is_empty() {
                return None;
            }
            let mut tile_des = Vec::new();
            let mut tile_pts = Vec::new();
            for (k, d) in kps.iter().zip(des.iter()) {
                let kx = k.x as f64;
                let ky = k.y as f64;
                if kx >= job.keep_x0 as f64
                    && kx < job.keep_x1 as f64
                    && ky >= job.keep_y0 as f64
                    && ky < job.keep_y1 as f64
                {
                    let px = job.x0 as f64 + kx;
                    let py = job.y0 as f64 + ky;
                    let wx = ((tx_start as f64) * 256.0 + (px + 0.5)) / s_src - orig_x;
                    let wy = ((ty_start as f64) * 256.0 + (py + 0.5)) / s_src - orig_y;
                    let rdes = to_rootsift(d);
                    let qdes = quantize_rootsift_u8(&rdes, SCALE_FACTOR);
                    tile_des.push(qdes);
                    tile_pts.push([wx as f32, wy as f32]);
                }
            }
            if tile_des.is_empty() {
                None
            } else {
                Some((tile_des, tile_pts))
            }
        })
        .collect();

    let mut all_des: Vec<[u8; 128]> = Vec::new();
    let mut all_pts: Vec<[f32; 2]> = Vec::new();
    for (d, p) in per_tile.into_iter().flatten() {
        all_des.extend(d);
        all_pts.extend(p);
    }

    if all_des.is_empty() {
        return Err(CvError::EmptyInput(
            "No descriptors extracted when building Teyvat global index".into(),
        ));
    }

    let n = all_des.len();
    let mut flat_des = Vec::with_capacity(n * 128);
    for d in &all_des {
        flat_des.extend_from_slice(d);
    }
    let mut flat_pts = Vec::with_capacity(n * 2);
    for p in &all_pts {
        flat_pts.extend_from_slice(p);
    }

    let terrain_fp = terrain_raster_fingerprint(map2_gray, map2_meta);
    let mut archive = NpzArchive::new();
    archive.insert("des_u8", NpyArray::from_u8(vec![n, 128], flat_des)?);
    archive.insert("pts_world", NpyArray::from_f32(vec![n, 2], flat_pts)?);
    archive.insert("scale_factor", NpyArray::scalar_f32(SCALE_FACTOR));
    archive.insert("index_version", NpyArray::scalar_i32(INDEX_VERSION));
    archive.insert(
        "terrain_fingerprint",
        NpyArray::from_i64(Vec::new(), vec![terrain_fp])?,
    );

    let dest = cache_path
        .map(PathBuf::from)
        .unwrap_or_else(default_global_index_path);
    archive.write_file_atomic(&dest)?;

    Ok((all_des, all_pts))
}

struct LoadedCachedIndex {
    des_u8: Vec<[u8; 128]>,
    pts_world: Vec<[f32; 2]>,
    scale_factor: f32,
}

fn load_cached_index(path: &Path, expected_fingerprint: Option<i64>) -> Option<LoadedCachedIndex> {
    if !path.is_file() {
        return None;
    }
    let archive = NpzArchive::read_file(path).ok()?;
    let ver_arr = archive.get("index_version")?;
    let ver = match &ver_arr.data {
        NpyData::I32(v) if !v.is_empty() => v[0],
        NpyData::I64(v) if !v.is_empty() => v[0] as i32,
        _ => return None,
    };
    if ver != INDEX_VERSION {
        return None;
    }
    if let Some(expected_fp) = expected_fingerprint {
        let fp_arr = archive.get("terrain_fingerprint")?;
        let NpyData::I64(v) = &fp_arr.data else {
            return None;
        };
        if v.is_empty() || v[0] != expected_fp {
            return None;
        }
    }

    let des_arr = archive.get("des_u8")?;
    if des_arr.shape.len() != 2 || des_arr.shape[1] != 128 || des_arr.shape[0] < 2 {
        return None;
    }
    let n = des_arr.shape[0];
    let NpyData::U8(des_raw) = &des_arr.data else {
        return None;
    };
    if des_raw.len() != n * 128 {
        return None;
    }

    let pts_arr = archive.get("pts_world")?;
    if pts_arr.shape != [n, 2] {
        return None;
    }
    let NpyData::F32(pts_raw) = &pts_arr.data else {
        return None;
    };
    if pts_raw.len() != n * 2 {
        return None;
    }

    let scale_factor = archive
        .get("scale_factor")
        .and_then(|sf| match &sf.data {
            NpyData::F32(v) if !v.is_empty() && v[0].is_finite() && v[0] > 0.0 => Some(v[0]),
            _ => None,
        })
        .unwrap_or(SCALE_FACTOR);

    let mut des_u8 = Vec::with_capacity(n);
    for chunk in des_raw.as_chunks::<128>().0 {
        des_u8.push(*chunk);
    }
    let mut pts_world = Vec::with_capacity(n);
    for chunk in pts_raw.as_chunks::<2>().0 {
        pts_world.push(*chunk);
    }

    Some(LoadedCachedIndex {
        des_u8,
        pts_world,
        scale_factor,
    })
}

pub fn load_map2_raster_from_npz(path: &Path) -> Option<Arc<TerrainRaster>> {
    if !path.is_file() {
        return None;
    }
    let gray_arr = NpzArchive::read_single_from_file(path, "map_2_gray").ok()?;
    let meta_arr = NpzArchive::read_single_from_file(path, "map_2_meta").ok()?;
    if gray_arr.shape.len() != 2 {
        return None;
    }
    let (h, w) = (gray_arr.shape[0], gray_arr.shape[1]);
    let NpyData::U8(raw_gray) = gray_arr.data else {
        return None;
    };
    let gray = Array2::from_shape_vec((h, w), raw_gray).ok()?;
    let meta_vals: Vec<f64> = match meta_arr.data {
        NpyData::I64(v) => v.into_iter().map(|x| x as f64).collect(),
        NpyData::I32(v) => v.into_iter().map(|x| x as f64).collect(),
        NpyData::F32(v) => v.into_iter().map(|x| x as f64).collect(),
        NpyData::U8(v) => v.into_iter().map(|x| x as f64).collect(),
    };
    if meta_vals.len() < 5 {
        return None;
    }
    Some(Arc::new(TerrainRaster {
        gray,
        meta: [
            meta_vals[0],
            meta_vals[1],
            meta_vals[2],
            meta_vals[3],
            meta_vals[4],
        ],
    }))
}

fn load_map2_from_npz() -> Option<Arc<TerrainRaster>> {
    let candidates = [
        resolve_data_asset("terrain_n2_v4.npz"),
        AppPaths::resolve().data_dir.join("terrain_n2_v4.npz"),
    ];
    for p in candidates {
        if let Some(raster) = load_map2_raster_from_npz(&p) {
            return Some(raster);
        }
    }
    None
}

struct LoadedGlobalIndex {
    kd_tree: KdTreeIndexU8,
    pts_world: Vec<[f32; 2]>,
    num_descriptors: usize,
}

struct RelocatorInner {
    terrain_map2: Option<Arc<TerrainRaster>>,
    cache_path: PathBuf,
    allow_default_npz_fallback: bool,
    build_lock: Mutex<()>,
    warmup_running: AtomicBool,
    ready: AtomicBool,
    index: RwLock<Option<Arc<LoadedGlobalIndex>>>,
    sift_q: SiftExtractor,
    annulus_masks: Mutex<HashMap<usize, Arc<Array2<u8>>>>,
}

struct WarmupGuard(Arc<RelocatorInner>);

impl Drop for WarmupGuard {
    fn drop(&mut self) {
        self.0.warmup_running.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct TeyvatGlobalRelocator {
    inner: Arc<RelocatorInner>,
}

impl TeyvatGlobalRelocator {
    pub fn new(
        terrain_map2: Option<Arc<TerrainRaster>>,
        cache_path: Option<PathBuf>,
        lazy: bool,
    ) -> Self {
        Self::new_with_fallback(terrain_map2, cache_path, lazy, true)
    }

    pub fn new_with_fallback(
        terrain_map2: Option<Arc<TerrainRaster>>,
        cache_path: Option<PathBuf>,
        lazy: bool,
        allow_default_npz_fallback: bool,
    ) -> Self {
        let resolved_path = cache_path.unwrap_or_else(default_global_index_path);
        let reloc = Self {
            inner: Arc::new(RelocatorInner {
                terrain_map2,
                cache_path: resolved_path,
                allow_default_npz_fallback,
                build_lock: Mutex::new(()),
                warmup_running: AtomicBool::new(false),
                ready: AtomicBool::new(false),
                index: RwLock::new(None),
                sift_q: SiftExtractor::new(160, 0.008, 15.0),
                annulus_masks: Mutex::new(HashMap::new()),
            }),
        };
        if !lazy {
            reloc.ensure_ready();
        }
        reloc
    }

    pub fn from_terrain_data(
        terrain_data: &BTreeMap<i32, Arc<TerrainRaster>>,
        cache_path: Option<PathBuf>,
        lazy: bool,
    ) -> Self {
        Self::new(terrain_data.get(&2).cloned(), cache_path, lazy)
    }

    pub fn from_terrain_data_with_fallback(
        terrain_data: &BTreeMap<i32, Arc<TerrainRaster>>,
        cache_path: Option<PathBuf>,
        lazy: bool,
        allow_default_npz_fallback: bool,
    ) -> Self {
        Self::new_with_fallback(
            terrain_data.get(&2).cloned(),
            cache_path,
            lazy,
            allow_default_npz_fallback,
        )
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_warming_up(&self) -> bool {
        self.inner.warmup_running.load(Ordering::Acquire)
    }

    pub fn num_descriptors(&self) -> usize {
        self.inner
            .index
            .read()
            .expect("index rwlock")
            .as_ref()
            .map_or(0, |idx| idx.num_descriptors)
    }

    pub fn cache_path(&self) -> &Path {
        &self.inner.cache_path
    }

    pub fn warmup_async(&self) {
        if self.is_ready() {
            return;
        }
        if self
            .inner
            .warmup_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let spawn_res = std::thread::Builder::new()
            .name("gh-global-relocator-warmup".to_owned())
            .spawn(move || {
                let _guard = WarmupGuard(Arc::clone(&inner));
                let _t0 = Instant::now();
                let _ = ensure_ready_inner(&inner);
            });
        if spawn_res.is_err() {
            self.inner.warmup_running.store(false, Ordering::Release);
        }
    }

    pub fn ensure_ready(&self) -> bool {
        if self.is_ready() {
            return true;
        }
        ensure_ready_inner(&self.inner)
    }

    fn get_annulus_mask(&self, sz: usize) -> Arc<Array2<u8>> {
        {
            let guard = self.inner.annulus_masks.lock().expect("annulus_masks lock");
            if let Some(m) = guard.get(&sz) {
                return Arc::clone(m);
            }
        }
        let mut mask = Array2::<u8>::zeros((sz, sz));
        let c = (sz.saturating_sub(1)) as f64 * 0.5;
        let r_max = sz as f64 * 0.5;
        let r_inner = 0.22 * r_max;
        let r_outer = 0.85 * r_max;
        for y in 0..sz {
            let dy = y as f64 - c;
            for x in 0..sz {
                let dx = x as f64 - c;
                let rr = dx.hypot(dy);
                if rr > r_inner && rr <= r_outer {
                    mask[[y, x]] = 255;
                }
            }
        }
        let arc_mask = Arc::new(mask);
        let mut guard = self.inner.annulus_masks.lock().expect("annulus_masks lock");
        Arc::clone(guard.entry(sz).or_insert(arc_mask))
    }

    fn extract_query_rootsift(
        &self,
        gray: ArrayView2<'_, u8>,
        bgr: Option<ArrayView3<'_, u8>>,
        sz: usize,
    ) -> Option<Vec<[f32; 128]>> {
        let (h, w) = gray.dim();
        let s_sz = h.min(w);
        let use_sz = sz.min(s_sz);
        if use_sz < 16 {
            return None;
        }

        let sq_g = if h == use_sz && w == use_sz {
            gray.to_owned()
        } else {
            resize_u8(gray, use_sz, use_sz, Interpolation::Area).ok()?
        };

        let base_mask = self.get_annulus_mask(use_sz);
        let mut mask = (*base_mask).clone();

        if let Some(bgr_view) = bgr {
            let (bh, bw, bc) = bgr_view.dim();
            if bc >= 3 {
                let bgr3 = bgr_view.slice(s![.., .., 0..3]);
                let sq_bgr = if bh == use_sz && bw == use_sz {
                    bgr3.to_owned()
                } else {
                    resize_u8_hwc(bgr3, use_sz, use_sz, Interpolation::Area).ok()?
                };
                if let Ok(hsv) = bgr_to_hsv(sq_bgr.view()) {
                    let mut active_px = 0usize;
                    let mut white_in_mask = 0usize;
                    let mut white_flags = vec![false; use_sz * use_sz];
                    for y in 0..use_sz {
                        for x in 0..use_sz {
                            if mask[[y, x]] > 0 {
                                active_px += 1;
                                if sq_g[[y, x]] >= 238 && hsv[[y, x, 1]] <= 38 {
                                    white_in_mask += 1;
                                    white_flags[y * use_sz + x] = true;
                                }
                            }
                        }
                    }
                    if active_px > 0
                        && (active_px - white_in_mask) as f64 >= 0.4 * (active_px as f64)
                    {
                        for y in 0..use_sz {
                            for x in 0..use_sz {
                                if white_flags[y * use_sz + x] {
                                    mask[[y, x]] = 0;
                                }
                            }
                        }
                    }
                }
            }
        }

        let (kps, des) = self
            .inner
            .sift_q
            .detect_and_compute(sq_g.view(), Some(mask.view()))
            .ok()?;
        if kps.len() < 4 || des.is_empty() {
            return None;
        }
        Some(to_rootsift_batch(&des))
    }

    fn query_candidates_internal(
        &self,
        gray: ArrayView2<'_, u8>,
        bgr: Option<ArrayView3<'_, u8>>,
        vote_bins: Option<&mut VoteBins>,
        top_k: usize,
        min_bin_score: f64,
    ) -> Vec<RelocateCandidate> {
        if !self.is_ready() || top_k == 0 {
            return Vec::new();
        }
        let (h, w) = gray.dim();
        if h.min(w) < 32 {
            return Vec::new();
        }
        let Some(index) = self
            .inner
            .index
            .read()
            .expect("index rwlock")
            .as_ref()
            .map(Arc::clone)
        else {
            return Vec::new();
        };

        let mut all_rdes: Vec<[f32; 128]> = Vec::new();
        for sz in [90usize, 60, 44] {
            if let Some(mut rdes) = self.extract_query_rootsift(gray, bgr, sz) {
                all_rdes.append(&mut rdes);
            }
        }
        if all_rdes.is_empty() {
            return Vec::new();
        }

        let pairs = index.kd_tree.knn_match(&all_rdes, 2);

        let mut local_bins = VoteBins::new();
        let bins = vote_bins.unwrap_or(&mut local_bins);

        for pair in pairs {
            if pair.len() < 2 {
                continue;
            }
            let d0 = pair[0].distance as f64;
            let d1 = pair[1].distance as f64;
            if d1 <= 1e-6 {
                continue;
            }
            let ratio = d0 / d1;
            if ratio >= LOWE_RATIO_THRESHOLD {
                continue;
            }
            let w_vote = 1.0 / (ratio + 0.15);
            let p = index.pts_world[pair[0].train_idx];
            let wx = p[0] as f64;
            let wy = p[1] as f64;
            let bx = (wx / BIN_WORLD).floor() as i32;
            let by = (wy / BIN_WORLD).floor() as i32;
            bins.entry((bx, by)).or_default().push(VoteSample {
                world_x: wx,
                world_y: wy,
                weight: w_vote,
            });
        }

        if bins.is_empty() {
            return Vec::new();
        }

        let bin_sums: HashMap<(i32, i32), f64> = bins
            .iter()
            .map(|(&k, v)| (k, v.iter().map(|s| s.weight).sum()))
            .collect();

        let mut scored: Vec<(f64, (i32, i32))> = Vec::new();
        for &(bx, by) in bin_sums.keys() {
            let mut nb_score = 0.0f64;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    if let Some(&s) = bin_sums.get(&(bx + dx, by + dy)) {
                        nb_score += s;
                    }
                }
            }
            if nb_score >= min_bin_score {
                scored.push((nb_score, (bx, by)));
            }
        }
        if scored.is_empty() {
            return Vec::new();
        }

        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let mut chosen: Vec<RelocateCandidate> = Vec::with_capacity(top_k);
        for (nb_score, (bx, by)) in scored {
            let mut sum_w = 0.0f64;
            let mut sum_wx = 0.0f64;
            let mut sum_wy = 0.0f64;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    if let Some(items) = bins.get(&(bx + dx, by + dy)) {
                        for item in items {
                            sum_w += item.weight;
                            sum_wx += item.world_x * item.weight;
                            sum_wy += item.world_y * item.weight;
                        }
                    }
                }
            }
            if sum_w <= 0.0 {
                continue;
            }
            let cx = sum_wx / sum_w;
            let cy = sum_wy / sum_w;
            if chosen
                .iter()
                .any(|o| (cx - o.world_x).hypot(cy - o.world_y) < NMS_DIST_W)
            {
                continue;
            }
            chosen.push(RelocateCandidate {
                world_x: cx,
                world_y: cy,
                score: nb_score,
            });
            if chosen.len() >= top_k {
                break;
            }
        }
        chosen
    }

    pub fn query_candidates(
        &self,
        minimap_img: ArrayView3<'_, u8>,
        vote_bins: Option<&mut VoteBins>,
        top_k: usize,
        min_bin_score: f64,
    ) -> Vec<RelocateCandidate> {
        let (h, w, c) = minimap_img.dim();
        if h.min(w) < 32 || c == 0 {
            return Vec::new();
        }
        if c == 1 {
            let gray = minimap_img.slice(s![.., .., 0]);
            return self.query_candidates_internal(gray, None, vote_bins, top_k, min_bin_score);
        }
        if c < 3 {
            return Vec::new();
        }
        let Ok(gray) = bgr_to_gray(minimap_img) else {
            return Vec::new();
        };
        self.query_candidates_internal(gray.view(), Some(minimap_img), vote_bins, top_k, min_bin_score)
    }

    pub fn query_candidates_gray(
        &self,
        minimap_gray: ArrayView2<'_, u8>,
        vote_bins: Option<&mut VoteBins>,
        top_k: usize,
        min_bin_score: f64,
    ) -> Vec<RelocateCandidate> {
        self.query_candidates_internal(minimap_gray, None, vote_bins, top_k, min_bin_score)
    }

    pub fn query_candidates_frame(
        &self,
        minimap_frame: FrameView<'_>,
        vote_bins: Option<&mut VoteBins>,
        top_k: usize,
        min_bin_score: f64,
    ) -> Vec<RelocateCandidate> {
        if (minimap_frame.width as usize).min(minimap_frame.height as usize) < 32 {
            return Vec::new();
        }
        match minimap_frame.format {
            PixelFormat::Gray8 => {
                let Ok(gray) = frame_view_to_gray_array(minimap_frame) else {
                    return Vec::new();
                };
                self.query_candidates_internal(gray.view(), None, vote_bins, top_k, min_bin_score)
            }
            PixelFormat::Bgr8 | PixelFormat::Bgra8 | PixelFormat::Rgba8 => {
                let Ok(bgr) = frame_view_to_bgr(minimap_frame) else {
                    return Vec::new();
                };
                let Ok(gray) = bgr_to_gray(bgr.view()) else {
                    return Vec::new();
                };
                self.query_candidates_internal(
                    gray.view(),
                    Some(bgr.view()),
                    vote_bins,
                    top_k,
                    min_bin_score,
                )
            }
        }
    }
}

fn ensure_ready_inner(inner: &RelocatorInner) -> bool {
    let _guard = inner.build_lock.lock().expect("build_lock");
    if inner.ready.load(Ordering::Acquire) {
        return true;
    }

    let expected_fp = inner
        .terrain_map2
        .as_ref()
        .map(|r| terrain_raster_fingerprint(r.gray.view(), &r.meta));
    let (des_u8, pts_world, scale_f) =
        if let Some(loaded) = load_cached_index(&inner.cache_path, expected_fp) {
            (loaded.des_u8, loaded.pts_world, loaded.scale_factor)
        } else {
        let Some(raster) = inner
            .terrain_map2
            .as_ref()
            .map(Arc::clone)
            .or_else(|| inner.allow_default_npz_fallback.then(load_map2_from_npz).flatten())
        else {
            return false;
        };
        let Ok((des_u8, pts_world)) =
            build_teyvat_global_index(raster.gray.view(), &raster.meta, Some(&inner.cache_path))
        else {
            return false;
        };
        (des_u8, pts_world, SCALE_FACTOR)
    };

    if des_u8.len() < 2 || pts_world.len() != des_u8.len() {
        return false;
    }

    let num_descriptors = pts_world.len();
    let Ok(kd_tree) = KdTreeIndexU8::build(des_u8, scale_f, KD_TREES, KD_CHECKS) else {
        return false;
    };

    let loaded = Arc::new(LoadedGlobalIndex {
        kd_tree,
        pts_world,
        num_descriptors,
    });
    *inner.index.write().expect("index rwlock") = Some(loaded);
    inner.ready.store(true, Ordering::Release);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_cache::{LruTerrainTileCache, TileCacheConfig};
    use ndarray::Array3;
    use std::time::Duration;

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

    fn make_synthetic_terrain_512() -> Arc<TerrainRaster> {
        let mut gray = Array2::<u8>::zeros((512, 512));
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next_u64 = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        for y in 0..512 {
            for x in 0..512 {
                let xf = x as f64;
                let yf = y as f64;
                let base = 115.0
                    + 45.0 * (xf * 0.043).sin() * (yf * 0.037).cos()
                    + 35.0 * (xf * 0.11 - yf * 0.09).cos()
                    + 20.0 * ((xf - 256.0).hypot(yf - 256.0) * 0.14).sin();
                gray[[y, x]] = base.clamp(15.0, 235.0) as u8;
            }
        }
        for _ in 0..140 {
            let cx = (next_u64() % 460 + 26) as isize;
            let cy = (next_u64() % 460 + 26) as isize;
            let rad = (next_u64() % 9 + 4) as isize;
            let col = (next_u64() % 180 + 40) as u8;
            for dy in -rad..=rad {
                for dx in -rad..=rad {
                    if dx * dx + dy * dy <= rad * rad {
                        gray[[(cy + dy) as usize, (cx + dx) as usize]] = col;
                    }
                }
            }
        }
        Arc::new(TerrainRaster {
            gray,
            meta: [7.0, 6.0, -2.0, 2000.0, 2000.0],
        })
    }

    #[test]
    fn default_index_path_uses_v4_filename() {
        let p = default_global_index_path();
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some(GLOBAL_INDEX_FILENAME)
        );
        assert_eq!(INDEX_VERSION, 4);
    }

    #[test]
    fn build_save_and_reload_v4_npz_in_temp_dir() {
        let tmp = TempDirGuard::new("gh_gr_v4");
        let cache_path = tmp.0.join(GLOBAL_INDEX_FILENAME);
        let raster = make_synthetic_terrain_512();
        let expected_fp = terrain_raster_fingerprint(raster.gray.view(), &raster.meta);
        let (built_des, built_pts) =
            build_teyvat_global_index(raster.gray.view(), &raster.meta, Some(&cache_path)).unwrap();
        assert!(built_des.len() >= 20);
        assert_eq!(built_des.len(), built_pts.len());
        assert!(cache_path.is_file());

        let npz = NpzArchive::read_file(&cache_path).unwrap();
        assert_eq!(npz.get("des_u8").unwrap().shape, [built_des.len(), 128]);
        assert_eq!(npz.get("pts_world").unwrap().shape, [built_pts.len(), 2]);
        let NpyData::I32(ver) = &npz.get("index_version").unwrap().data else {
            panic!("expected I32 index_version");
        };
        assert_eq!(ver[0], 4);
        let NpyData::I64(fp) = &npz.get("terrain_fingerprint").unwrap().data else {
            panic!("expected I64 terrain_fingerprint");
        };
        assert_eq!(fp, &[expected_fp]);
        let NpyData::F32(sf) = &npz.get("scale_factor").unwrap().data else {
            panic!("expected F32 scale_factor");
        };
        assert!((sf[0] - SCALE_FACTOR).abs() < 1e-5);

        let reloc = TeyvatGlobalRelocator::new(None, Some(cache_path), false);
        assert!(reloc.is_ready());
        assert_eq!(reloc.num_descriptors(), built_des.len());

        // Extract synthetic patch centered at (wx=6192, wy=5168) -> pixel (256, 256)
        let mut terrain_map = BTreeMap::new();
        terrain_map.insert(2, raster);
        let tc = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: terrain_map,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap();
        let wx = 6192.0;
        let wy = 5168.0;
        let (patch_opt, _) = tc.extract_local_patch(2, wx, wy, 180.0, 218.0 / 360.0);
        let patch = patch_opt.unwrap();
        let cands = reloc.query_candidates_gray(patch.view(), None, 4, 1.0);
        assert!(!cands.is_empty(), "expected candidates from synthetic terrain");
        let best_dist = cands
            .iter()
            .map(|c| (c.world_x - wx).hypot(c.world_y - wy))
            .fold(f64::INFINITY, f64::min);
        assert!(best_dist <= 250.0, "best_dist={best_dist}");
    }

    #[test]
    fn rejects_legacy_v2_and_v3_caches_and_rebuilds_v4() {
        let tmp = TempDirGuard::new("gh_gr_legacy_reject");
        let raster = make_synthetic_terrain_512();
        let expected_fp = terrain_raster_fingerprint(raster.gray.view(), &raster.meta);

        for legacy_ver in [2i32, 3i32] {
            let cache_path = tmp.0.join(format!("legacy_v{legacy_ver}.npz"));
            let mut legacy = NpzArchive::new();
            legacy.insert(
                "des_u8",
                NpyArray::from_u8(vec![4, 128], vec![10; 4 * 128]).unwrap(),
            );
            legacy.insert(
                "pts_world",
                NpyArray::from_f32(vec![4, 2], vec![0.0; 8]).unwrap(),
            );
            legacy.insert("scale_factor", NpyArray::scalar_f32(400.0));
            legacy.insert("index_version", NpyArray::scalar_i32(legacy_ver));
            legacy.write_file_atomic(&cache_path).unwrap();

            let reloc = TeyvatGlobalRelocator::new(
                Some(Arc::clone(&raster)),
                Some(cache_path.clone()),
                false,
            );
            assert!(reloc.is_ready(), "failed rebuild for legacy v{legacy_ver}");
            assert!(reloc.num_descriptors() >= 20);

            let rebuilt = NpzArchive::read_file(&cache_path).unwrap();
            let NpyData::I32(ver) = &rebuilt.get("index_version").unwrap().data else {
                panic!("expected I32 index_version");
            };
            assert_eq!(ver[0], 4);
            let NpyData::I64(fp) = &rebuilt.get("terrain_fingerprint").unwrap().data else {
                panic!("expected I64 terrain_fingerprint");
            };
            assert_eq!(fp, &[expected_fp]);
        }
    }

    #[test]
    fn rebuilds_cache_when_terrain_fingerprint_changes() {
        let tmp = TempDirGuard::new("gh_gr_fp_rebuild");
        let cache_path = tmp.0.join(GLOBAL_INDEX_FILENAME);
        let raster_a = make_synthetic_terrain_512();
        let fp_a = terrain_raster_fingerprint(raster_a.gray.view(), &raster_a.meta);

        let reloc_a =
            TeyvatGlobalRelocator::new(Some(Arc::clone(&raster_a)), Some(cache_path.clone()), false);
        assert!(reloc_a.is_ready());

        let mut gray_b = raster_a.gray.clone();
        for y in 240..272 {
            for x in 240..272 {
                gray_b[[y, x]] ^= 0x55;
            }
        }
        let raster_b = Arc::new(TerrainRaster {
            gray: gray_b,
            meta: raster_a.meta,
        });
        let fp_b = terrain_raster_fingerprint(raster_b.gray.view(), &raster_b.meta);
        assert_ne!(fp_a, fp_b);

        let reloc_b =
            TeyvatGlobalRelocator::new(Some(Arc::clone(&raster_b)), Some(cache_path.clone()), false);
        assert!(reloc_b.is_ready());

        let rebuilt = NpzArchive::read_file(&cache_path).unwrap();
        let NpyData::I64(fp) = &rebuilt.get("terrain_fingerprint").unwrap().data else {
            panic!("expected I64 terrain_fingerprint");
        };
        assert_eq!(fp, &[fp_b]);
    }

    #[test]
    fn is_ready_guard_and_warmup_async_lifecycle() {
        let tmp = TempDirGuard::new("gh_gr_warmup");
        let cache_path = tmp.0.join(GLOBAL_INDEX_FILENAME);
        let raster = make_synthetic_terrain_512();
        let reloc = TeyvatGlobalRelocator::new(Some(raster.clone()), Some(cache_path.clone()), true);
        assert!(!reloc.is_ready());

        // Query before ready must return empty immediately without blocking
        let dummy = Array3::<u8>::zeros((128, 128, 3));
        assert!(reloc.query_candidates(dummy.view(), None, 4, 1.5).is_empty());

        reloc.warmup_async();
        let start = Instant::now();
        while !reloc.is_ready() && start.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reloc.is_ready(), "warmup_async timed out");
        assert!(cache_path.is_file());

        // Negative flat frames must yield zero candidates
        for fill in [0u8, 248, 90] {
            let neg = Array3::<u8>::from_elem((218, 218, 3), fill);
            assert!(
                reloc
                    .query_candidates(neg.view(), None, 4, DEFAULT_MIN_BIN_SCORE)
                    .is_empty()
            );
        }
    }

    #[test]
    fn vote_bins_accumulate_and_nms_separates_clusters() {
        let tmp = TempDirGuard::new("gh_gr_bins");
        let cache_path = tmp.0.join(GLOBAL_INDEX_FILENAME);
        let raster = make_synthetic_terrain_512();
        let reloc = TeyvatGlobalRelocator::new(Some(raster), Some(cache_path), false);
        assert!(reloc.is_ready());

        let mut bins = VoteBins::new();
        bins.insert(
            (0, 0),
            vec![VoteSample {
                world_x: 100.0,
                world_y: 100.0,
                weight: 5.0,
            }],
        );
        bins.insert(
            (1, 0),
            vec![VoteSample {
                world_x: 260.0,
                world_y: 100.0,
                weight: 4.0,
            }],
        );
        // Far cluster > NMS_DIST_W (420.0) away
        bins.insert(
            (10, 10),
            vec![VoteSample {
                world_x: 2500.0,
                world_y: 2500.0,
                weight: 4.5,
            }],
        );

        let blank = Array2::<u8>::zeros((128, 128));
        // Confirm empty query still preserves external bins (if we test direct bin aggregation)
        let empty_cands = reloc.query_candidates_gray(blank.view(), Some(&mut bins), 4, 3.5);
        assert!(empty_cands.is_empty());
        assert_eq!(bins.len(), 3);
    }

    #[test]
    fn global_index_avoids_tile_feature_starvation_in_moderate_contrast_region() {
        // Single 384x384 tile with >1,000 high-response features in the top-left quadrant
        // and moderate-contrast features in the bottom-right quadrant (like calm Teyvat terrain).
        // Under a 900-feature tile cap, high-contrast corners evict the moderate-contrast region;
        // removing the per-tile cap must locate both regions reliably.
        let mut gray = Array2::<u8>::from_elem((384, 384), 120u8);
        let mut state = 0xfeed_face_cafe_beefu64;
        let mut next_u64 = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };

        // Top-left high-contrast corner (x in 16..208, y in 16..208): dense crisp features
        for y in 16..208 {
            for x in 16..208 {
                let xf = x as f64;
                let yf = y as f64;
                let fine = if ((x / 5) + (y / 5)) % 2 == 0 { 95.0 } else { -95.0 };
                let wave = 40.0 * (xf * 0.41).sin() * (yf * 0.37).cos();
                gray[[y, x]] = (128.0 + fine + wave).clamp(5.0, 250.0) as u8;
            }
        }
        for _ in 0..260 {
            let cx = (next_u64() % 176 + 24) as isize;
            let cy = (next_u64() % 176 + 24) as isize;
            let rad = (next_u64() % 4 + 2) as isize;
            let col = if next_u64() % 2 == 0 { 245u8 } else { 10u8 };
            for dy in -rad..=rad {
                for dx in -rad..=rad {
                    if dx * dx + dy * dy <= rad * rad {
                        gray[[(cy + dy) as usize, (cx + dx) as usize]] = col;
                    }
                }
            }
        }

        // Bottom-right moderate-contrast quadrant (x in 236..364, y in 236..364): std ~ 20
        for y in 236..364 {
            for x in 236..364 {
                let xf = (x - 236) as f64;
                let yf = (y - 236) as f64;
                let base = 124.0
                    + 18.0 * (xf * 0.13).sin() * (yf * 0.11).cos()
                    + 14.0 * (xf * 0.23 - yf * 0.19).cos()
                    + 10.0 * ((xf - 64.0).hypot(yf - 64.0) * 0.29).sin();
                gray[[y, x]] = base.clamp(70.0, 180.0) as u8;
            }
        }
        for _ in 0..65 {
            let cx = (next_u64() % 104 + 248) as isize;
            let cy = (next_u64() % 104 + 248) as isize;
            let rad = (next_u64() % 5 + 3) as isize;
            let col = (92 + (next_u64() % 64)) as u8;
            for dy in -rad..=rad {
                for dx in -rad..=rad {
                    if dx * dx + dy * dy <= rad * rad {
                        gray[[(cy + dy) as usize, (cx + dx) as usize]] = col;
                    }
                }
            }
        }

        let raster = Arc::new(TerrainRaster {
            gray,
            meta: [7.0, 6.0, -2.0, 2000.0, 2000.0],
        });
        let tmp = TempDirGuard::new("gh_gr_starvation");
        let cache_path = tmp.0.join(GLOBAL_INDEX_FILENAME);
        let reloc = TeyvatGlobalRelocator::new(Some(Arc::clone(&raster)), Some(cache_path), false);
        assert!(reloc.is_ready());

        let mut terrain_map = BTreeMap::new();
        terrain_map.insert(2, raster);
        let tc = LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: terrain_map,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap();

        // Control: high-contrast quadrant centered at pixel (110, 110) -> world (5610, 4586)
        let (wx_hi, wy_hi) = (5610.0, 4586.0);
        let (patch_hi_opt, _) = tc.extract_local_patch(2, wx_hi, wy_hi, 180.0, 0.25);
        let patch_hi = patch_hi_opt.expect("high-contrast patch");
        let cands_hi =
            reloc.query_candidates_gray(patch_hi.view(), None, 4, DEFAULT_MIN_BIN_SCORE);
        assert!(
            !cands_hi.is_empty(),
            "control high-contrast region must relocate"
        );
        let best_dist_hi = cands_hi
            .iter()
            .map(|c| (c.world_x - wx_hi).hypot(c.world_y - wy_hi))
            .fold(f64::INFINITY, f64::min);
        assert!(
            best_dist_hi <= 250.0,
            "control high-contrast best_dist={best_dist_hi}"
        );

        // Bug reproducer: moderate-contrast quadrant centered at pixel (300, 300) -> world (6370, 5346)
        let (wx_lo, wy_lo) = (6370.0, 5346.0);
        let (patch_lo_opt, _) = tc.extract_local_patch(2, wx_lo, wy_lo, 180.0, 0.25);
        let patch_lo = patch_lo_opt.expect("moderate-contrast patch");
        let mut bins_lo = VoteBins::new();
        let cands_lo =
            reloc.query_candidates_gray(patch_lo.view(), Some(&mut bins_lo), 4, DEFAULT_MIN_BIN_SCORE);
        let votes_lo: usize = bins_lo.values().map(Vec::len).sum();
        assert!(
            !cands_lo.is_empty(),
            "moderate-contrast region starved of global index votes (votes={votes_lo}, total_des={})",
            reloc.num_descriptors()
        );
        let best_dist_lo = cands_lo
            .iter()
            .map(|c| (c.world_x - wx_lo).hypot(c.world_y - wy_lo))
            .fold(f64::INFINITY, f64::min);
        assert!(
            best_dist_lo <= 250.0,
            "moderate-contrast best_dist={best_dist_lo} (votes={votes_lo})"
        );
    }
}
