//! Non-blocking (`async_fetch = true`) `HoyolabHdTileProvider` & `UndergroundOverlayProvider`
//! implementing `gh_core::traits::{TileProvider, UndergroundOverlaySource}` (`src/hd_assets.py`).

use gh_core::atomic_io;
use gh_core::frame::{FrameBuf, PixelFormat};
use gh_core::multimap::{DEFAULT_TILE_BASE, FloorOverlayRecord, MultimapDb};
use gh_core::paths::AppPaths;
use gh_core::traits::{TileProvider, UndergroundOverlaySource};
use gh_core::{CoreError, CoreResult};
use image::imageops::FilterType;
use image::{ColorType, ImageReader};
use reqwest::StatusCode;
use reqwest::header::USER_AGENT;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

pub use gh_core::multimap::{load_multimap_db, resolve_multimap_json_path};

pub const TILE_BASE: &str = DEFAULT_TILE_BASE;
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
pub const NET_BACKOFF_SECONDS: f64 = 60.0;
pub const MAX_TRANSIENT_ERRORS: u32 = 3;
pub const MAX_UG_MEM_CACHE_ENTRIES: usize = 8;
pub const MAX_UG_MEM_CACHE_BYTES: usize = 12 * 1024 * 1024;

pub fn default_hd_tile_dir() -> PathBuf {
    AppPaths::resolve().tiles_dir
}

pub fn default_ug_overlay_dir() -> PathBuf {
    AppPaths::resolve().ug_overlays_dir
}

pub fn default_paimon_cache_path() -> PathBuf {
    AppPaths::resolve().paimon_cache_path()
}

#[derive(Debug, Clone)]
pub struct HdTileConfig {
    pub disk_dir: PathBuf,
    pub maps_db: Arc<MultimapDb>,
    pub tile_base: String,
    pub timeout: Duration,
    pub async_fetch: bool,
    pub offline: bool,
    pub max_workers: usize,
}

impl Default for HdTileConfig {
    fn default() -> Self {
        Self {
            disk_dir: default_hd_tile_dir(),
            maps_db: Arc::new(MultimapDb::default()),
            tile_base: TILE_BASE.to_owned(),
            timeout: Duration::from_secs(5),
            async_fetch: true,
            offline: false,
            max_workers: 4,
        }
    }
}

type TileKey = (i32, i32, i32, i32);

#[derive(Debug, Default)]
struct HdTileState {
    closed: bool,
    missing: HashSet<TileKey>,
    inflight: HashSet<TileKey>,
    consecutive_errors: u32,
    net_backoff_until: Option<Instant>,
}

struct HdTileInner {
    disk_dir: PathBuf,
    maps_db: Arc<MultimapDb>,
    tile_base: String,
    timeout: Duration,
    async_fetch: bool,
    offline: bool,
    max_workers: usize,
    state: Mutex<HdTileState>,
    pool: Mutex<Option<mpsc::Sender<TileKey>>>,
    downloads: AtomicU64,
    disk_hits: AtomicU64,
    tile_generation: AtomicU64,
}

#[derive(Clone)]
pub struct HoyolabHdTileProvider {
    inner: Arc<HdTileInner>,
}

impl std::fmt::Debug for HoyolabHdTileProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HoyolabHdTileProvider")
            .field("disk_dir", &self.inner.disk_dir)
            .field("async_fetch", &self.inner.async_fetch)
            .field("offline", &self.inner.offline)
            .field("downloads", &self.downloads())
            .field("disk_hits", &self.disk_hits())
            .finish()
    }
}

impl HoyolabHdTileProvider {
    pub fn from_config(cfg: HdTileConfig) -> CoreResult<Self> {
        std::fs::create_dir_all(&cfg.disk_dir)?;
        let tile_base = cfg.tile_base.trim().trim_end_matches('/').to_owned();
        Ok(Self {
            inner: Arc::new(HdTileInner {
                disk_dir: cfg.disk_dir,
                maps_db: cfg.maps_db,
                tile_base: if tile_base.is_empty() {
                    TILE_BASE.to_owned()
                } else {
                    tile_base
                },
                timeout: cfg.timeout.max(Duration::from_millis(50)),
                async_fetch: cfg.async_fetch,
                offline: cfg.offline,
                max_workers: cfg.max_workers.max(1),
                state: Mutex::new(HdTileState::default()),
                pool: Mutex::new(None),
                downloads: AtomicU64::new(0),
                disk_hits: AtomicU64::new(0),
                tile_generation: AtomicU64::new(0),
            }),
        })
    }

    pub fn new(
        disk_dir: Option<PathBuf>,
        maps_db: Option<Arc<MultimapDb>>,
        timeout: Duration,
        async_fetch: bool,
        offline: bool,
        max_workers: usize,
    ) -> CoreResult<Self> {
        let dir = disk_dir.unwrap_or_else(default_hd_tile_dir);
        let db = maps_db.unwrap_or_else(|| {
            let p = resolve_multimap_json_path(None, None);
            Arc::new(load_multimap_db(&p).unwrap_or_default())
        });
        Self::from_config(HdTileConfig {
            disk_dir: dir,
            maps_db: db,
            tile_base: TILE_BASE.to_owned(),
            timeout,
            async_fetch,
            offline,
            max_workers,
        })
    }

    pub fn downloads(&self) -> u64 {
        self.inner.downloads.load(Ordering::Acquire)
    }

    pub fn disk_hits(&self) -> u64 {
        self.inner.disk_hits.load(Ordering::Relaxed)
    }

    /// Monotonic counter incremented whenever a new tile is saved to disk.
    pub fn tile_generation(&self) -> u64 {
        self.inner.tile_generation.load(Ordering::Acquire)
    }

    pub fn disk_dir(&self) -> &Path {
        &self.inner.disk_dir
    }

    pub fn maps_db(&self) -> &Arc<MultimapDb> {
        &self.inner.maps_db
    }

    pub fn tile_paths(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> (PathBuf, PathBuf) {
        self.inner.tile_paths(map_id, zoom, tx, ty)
    }

    /// Unified color tile disk path (`{disk_dir}/{map_id}/{zoom}/{tx}_{ty}.webp`) shared by
    /// CV (`HoyolabHdTileProvider`), Native Live Radar Map, and LAN Companion.
    pub fn color_tile_path(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> PathBuf {
        self.inner.color_tile_path(map_id, zoom, tx, ty)
    }

    pub fn get_tile_frame(
        &self,
        map_id: i32,
        zoom: i32,
        tx: i32,
        ty: i32,
        async_override: Option<bool>,
    ) -> Option<FrameBuf> {
        if !(-2..=0).contains(&zoom) || tx < 0 || ty < 0 {
            return None;
        }
        let key = (map_id, zoom, tx, ty);
        {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.missing.contains(&key) {
                return None;
            }
        }

        let webp_path = self.inner.color_tile_path(map_id, zoom, tx, ty);
        if webp_path.is_file()
            && let Ok(raw) = std::fs::read(&webp_path)
            && let Some(frame) = decode_tile_gray256(&raw)
        {
            self.inner.disk_hits.fetch_add(1, Ordering::Relaxed);
            return Some(frame);
        }

        // Legacy grayscale PNG fallback for pre-v5.7 caches already on disk.
        let (png_path, miss_path) = self.inner.tile_paths(map_id, zoom, tx, ty);
        if png_path.is_file()
            && let Ok(raw) = std::fs::read(&png_path)
            && let Some(frame) = decode_tile_gray256(&raw)
        {
            self.inner.disk_hits.fetch_add(1, Ordering::Relaxed);
            return Some(frame);
        }

        if miss_path.is_file() {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.missing.insert(key);
            return None;
        }

        if self.inner.offline {
            return None;
        }

        let effective_async = async_override.unwrap_or(self.inner.async_fetch);
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return None;
            }
            if let Some(until) = st.net_backoff_until
                && Instant::now() < until
            {
                return None;
            }
            if st.inflight.contains(&key) {
                return None;
            }
            st.inflight.insert(key);
        }

        if effective_async {
            if self.enqueue_job(key).is_err() {
                let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&key);
            }
            return None;
        }

        let inner = Arc::clone(&self.inner);
        run_blocking_http(move || {
            let http = build_blocking_client(inner.timeout);
            inner.fetch_and_cache(http.as_ref(), map_id, zoom, tx, ty)
        })
    }

    /// Reads cached full-color `.webp` tile bytes (zoom down to `detail_v2.min_zoom`), or
    /// enqueues an async download when missing. Zero duplicate downloads with CV or LAN Companion.
    pub fn get_color_tile_bytes(
        &self,
        map_id: i32,
        zoom: i32,
        tx: i32,
        ty: i32,
        async_override: Option<bool>,
    ) -> Option<Vec<u8>> {
        if tx < 0 || ty < 0 {
            return None;
        }
        let d2 = self.inner.maps_db.map(map_id)?.detail_v2.as_ref()?;
        if zoom < d2.min_zoom || zoom > d2.max_zoom {
            return None;
        }
        let (max_tx, max_ty) = d2.max_tile_xy(zoom);
        if tx > max_tx || ty > max_ty {
            return None;
        }

        let key = (map_id, zoom, tx, ty);
        {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.missing.contains(&key) {
                return None;
            }
        }

        let webp_path = self.inner.color_tile_path(map_id, zoom, tx, ty);
        if webp_path.is_file()
            && let Ok(raw) = std::fs::read(&webp_path)
            && raw.len() >= 12
        {
            self.inner.disk_hits.fetch_add(1, Ordering::Relaxed);
            return Some(raw);
        }

        let (_, miss_path) = self.inner.tile_paths(map_id, zoom, tx, ty);
        if miss_path.is_file() {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.missing.insert(key);
            return None;
        }

        if self.inner.offline {
            return None;
        }

        let effective_async = async_override.unwrap_or(self.inner.async_fetch);
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return None;
            }
            if let Some(until) = st.net_backoff_until
                && Instant::now() < until
            {
                return None;
            }
            if st.inflight.contains(&key) {
                return None;
            }
            st.inflight.insert(key);
        }

        if effective_async {
            if self.enqueue_job(key).is_err() {
                let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&key);
            }
            return None;
        }

        let inner = Arc::clone(&self.inner);
        run_blocking_http(move || {
            let http = build_blocking_client(inner.timeout);
            let _ = inner.fetch_and_cache(http.as_ref(), map_id, zoom, tx, ty);
            std::fs::read(inner.color_tile_path(map_id, zoom, tx, ty)).ok()
        })
    }

    /// Non-blocking prefetch of specific `(zoom, tx, ty)` full-color `.webp` tiles.
    pub fn prefetch_color_tiles_async(&self, map_id: i32, tiles: &[(i32, i32, i32)]) {
        if self.inner.offline || tiles.is_empty() {
            return;
        }
        let Some(d2) = self
            .inner
            .maps_db
            .map(map_id)
            .and_then(|m| m.detail_v2.as_ref())
        else {
            return;
        };
        for &(zoom, tx, ty) in tiles {
            if tx < 0 || ty < 0 || zoom < d2.min_zoom || zoom > d2.max_zoom {
                continue;
            }
            let (max_tx, max_ty) = d2.max_tile_xy(zoom);
            if tx > max_tx || ty > max_ty {
                continue;
            }
            let key = (map_id, zoom, tx, ty);
            if self.inner.color_tile_path(map_id, zoom, tx, ty).is_file() {
                continue;
            }
            let (_, miss_path) = self.inner.tile_paths(map_id, zoom, tx, ty);
            if miss_path.is_file() {
                let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                st.missing.insert(key);
                continue;
            }
            {
                let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                if st.closed || st.missing.contains(&key) || st.inflight.contains(&key) {
                    continue;
                }
                if let Some(until) = st.net_backoff_until
                    && Instant::now() < until
                {
                    break;
                }
                st.inflight.insert(key);
            }
            if self.enqueue_job(key).is_err() {
                let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&key);
            }
        }
    }

    /// Non-blocking warm-up of the ultra-light `min_zoom` coarse overview layer for `map_id`
    /// (e.g., 72 tiles for all of Teyvat at `z = -4`, 20 for Nod-Krai, 1 for sub-maps) so
    /// parent-tile fallback guarantees 0ms blank frames anywhere on the map.
    pub fn prefetch_coarse_overview_async(&self, map_id: i32) {
        if self.inner.offline {
            return;
        }
        let Some(d2) = self
            .inner
            .maps_db
            .map(map_id)
            .and_then(|m| m.detail_v2.as_ref())
        else {
            return;
        };
        let z = d2.min_zoom;
        let (max_tx, max_ty) = d2.max_tile_xy(z);
        let total = (max_tx + 1).saturating_mul(max_ty + 1);
        if total <= 0 || total > 144 {
            return;
        }
        let mut tiles = Vec::with_capacity(total as usize);
        for ty in 0..=max_ty {
            for tx in 0..=max_tx {
                tiles.push((z, tx, ty));
            }
        }
        self.prefetch_color_tiles_async(map_id, &tiles);
    }

    pub fn prefetch_region(
        &self,
        map_id: i32,
        world_rect: [f64; 4],
        zoom: i32,
        max_workers: usize,
    ) -> CoreResult<usize> {
        let [wx_min, wx_max, wy_min, wy_max] = world_rect;
        let Some(mdata) = self.inner.maps_db.map(map_id) else {
            return Ok(0);
        };
        let Some(d2) = &mdata.detail_v2 else {
            return Ok(0);
        };
        let (tx0, tx1, ty0, ty1) =
            d2.tile_range_for_world_rect(wx_min, wx_max, wy_min, wy_max, zoom);

        let mut jobs = Vec::new();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                jobs.push((map_id, zoom, tx, ty));
            }
        }
        if jobs.len() > 400 {
            return Err(CoreError::InvalidArgument(format!(
                "Refusing huge prefetch of {} tiles",
                jobs.len()
            )));
        }
        if jobs.is_empty() {
            return Ok(0);
        }

        let workers = max_workers.clamp(1, jobs.len());
        let loaded = AtomicU64::new(0);
        let queue = Mutex::new(jobs.into_iter());

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let next_job = {
                            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                            q.next()
                        };
                        let Some((mid, z, tx, ty)) = next_job else {
                            break;
                        };
                        if self.get_tile_frame(mid, z, tx, ty, Some(false)).is_some() {
                            loaded.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        Ok(loaded.load(Ordering::Relaxed) as usize)
    }

    pub fn close(&self) {
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
        }
        let mut pool = self.inner.pool.lock().unwrap_or_else(|e| e.into_inner());
        *pool = None;
    }

    fn enqueue_job(&self, key: (i32, i32, i32, i32)) -> Result<(), ()> {
        let mut pool_guard = self.inner.pool.lock().unwrap_or_else(|e| e.into_inner());
        {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return Err(());
            }
        }
        if pool_guard.is_none() {
            let (tx, rx) = mpsc::channel::<(i32, i32, i32, i32)>();
            let rx = Arc::new(Mutex::new(rx));
            for idx in 0..self.inner.max_workers {
                let inner = Arc::clone(&self.inner);
                let rx_clone = Arc::clone(&rx);
                let _ = std::thread::Builder::new()
                    .name(format!("gh-hd-tile-{idx}"))
                    .spawn(move || {
                        let http = build_blocking_client(inner.timeout);
                        loop {
                            let job = {
                                let guard = rx_clone.lock().unwrap_or_else(|e| e.into_inner());
                                guard.recv()
                            };
                            let Ok((mid, z, x, y)) = job else {
                                break;
                            };
                            let is_closed = {
                                let st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                                st.closed
                            };
                            if is_closed {
                                let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                                st.inflight.remove(&(mid, z, x, y));
                                continue;
                            }
                            let _ = inner.fetch_and_cache(http.as_ref(), mid, z, x, y);
                        }
                    });
            }
            *pool_guard = Some(tx);
        }
        if let Some(tx) = pool_guard.as_ref() {
            tx.send(key).map_err(|_| ())
        } else {
            Err(())
        }
    }
}

impl Drop for HoyolabHdTileProvider {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.close();
        }
    }
}

impl TileProvider for HoyolabHdTileProvider {
    fn request_tile(
        &self,
        map_id: i32,
        z: i32,
        x: i32,
        y: i32,
        async_fetch: bool,
    ) -> Option<Vec<u8>> {
        self.get_tile_frame(map_id, z, x, y, Some(async_fetch))
            .map(|frame| frame.data)
    }

    fn downloads(&self) -> u64 {
        self.downloads()
    }
}

impl HdTileInner {
    fn tile_paths(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> (PathBuf, PathBuf) {
        let sub = self.disk_dir.join(map_id.to_string()).join(zoom.to_string());
        (
            sub.join(format!("{tx}_{ty}.png")),
            sub.join(format!("{tx}_{ty}.404")),
        )
    }

    fn color_tile_path(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> PathBuf {
        self.disk_dir
            .join(map_id.to_string())
            .join(zoom.to_string())
            .join(format!("{tx}_{ty}.webp"))
    }

    fn record_transient_error(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.consecutive_errors = st.consecutive_errors.saturating_add(1);
        if st.consecutive_errors >= MAX_TRANSIENT_ERRORS {
            st.net_backoff_until =
                Some(Instant::now() + Duration::from_secs_f64(NET_BACKOFF_SECONDS));
        }
    }

    fn record_success(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.consecutive_errors = 0;
        st.net_backoff_until = None;
    }

    fn fetch_and_cache(
        &self,
        http: Option<&reqwest::blocking::Client>,
        mid: i32,
        z: i32,
        ix: i32,
        iy: i32,
    ) -> Option<FrameBuf> {
        struct InflightGuard<'a> {
            state: &'a Mutex<HdTileState>,
            key: (i32, i32, i32, i32),
        }
        impl Drop for InflightGuard<'_> {
            fn drop(&mut self) {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&self.key);
            }
        }

        let key = (mid, z, ix, iy);
        let _guard = InflightGuard {
            state: &self.state,
            key,
        };

        let mdata = self.maps_db.map(mid)?;
        let d2 = mdata.detail_v2.as_ref()?;
        if d2.map_version.trim().is_empty()
            || d2.total_size[0] <= 0.0
            || d2.total_size[1] <= 0.0
        {
            return None;
        }
        let (max_tx, max_ty) = d2.max_tile_xy(z);
        if ix > max_tx || iy > max_ty {
            return None;
        }

        let url = d2.tile_url(&self.tile_base, mid, z, ix, iy);
        let webp_path = self.color_tile_path(mid, z, ix, iy);
        let (_, miss_path) = self.tile_paths(mid, z, ix, iy);

        let client = match http {
            Some(c) => c,
            None => {
                self.record_transient_error();
                return None;
            }
        };

        let resp = match client
            .get(&url)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .timeout(self.timeout)
            .send()
        {
            Ok(r) => r,
            Err(_) => {
                self.record_transient_error();
                return None;
            }
        };

        let status = resp.status();
        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            if let Some(parent) = miss_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = atomic_io::write_bytes_atomic(&miss_path, b"");
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.missing.insert(key);
            return None;
        }
        if !status.is_success() {
            self.record_transient_error();
            return None;
        }

        let raw = match resp.bytes() {
            Ok(b) => b,
            Err(_) => {
                self.record_transient_error();
                return None;
            }
        };

        let Some(frame) = decode_tile_gray256(&raw) else {
            self.record_transient_error();
            return None;
        };

        // Store the original full-color `.webp` bytes once so CV, Native Radar Map, and LAN
        // Companion share one cache with zero duplicate downloads.
        if atomic_io::write_bytes_atomic(&webp_path, &raw).is_err() {
            self.record_transient_error();
            return None;
        }

        self.record_success();
        self.downloads.fetch_add(1, Ordering::Release);
        self.tile_generation.fetch_add(1, Ordering::AcqRel);
        Some(frame)
    }
}

#[derive(Debug, Clone)]
pub struct UndergroundOverlayConfig {
    pub disk_dir: PathBuf,
    pub maps_db: Arc<MultimapDb>,
    pub timeout: Duration,
    pub async_fetch: bool,
    pub offline: bool,
    pub max_workers: usize,
}

impl Default for UndergroundOverlayConfig {
    fn default() -> Self {
        Self {
            disk_dir: default_ug_overlay_dir(),
            maps_db: Arc::new(MultimapDb::default()),
            timeout: Duration::from_secs(8),
            async_fetch: true,
            offline: false,
            max_workers: 2,
        }
    }
}

#[derive(Clone)]
struct CachedOverlayBytes {
    raw_png: Arc<[u8]>,
    record: FloorOverlayRecord,
}

#[derive(Default)]
struct UgOverlayState {
    closed: bool,
    inflight: HashSet<(i32, i32, i32)>,
    consecutive_errors: u32,
    net_backoff_until: Option<Instant>,
    mem_cache: VecDeque<((i32, i32, i32), CachedOverlayBytes)>,
    mem_cache_bytes: usize,
}

impl UgOverlayState {
    fn get_cached(&mut self, key: &(i32, i32, i32)) -> Option<CachedOverlayBytes> {
        let pos = self.mem_cache.iter().position(|(k, _)| k == key)?;
        let entry = self.mem_cache.remove(pos)?;
        let out = entry.1.clone();
        self.mem_cache.push_back(entry);
        Some(out)
    }

    fn insert_mem_cache(&mut self, key: (i32, i32, i32), entry: CachedOverlayBytes) {
        if let Some(pos) = self.mem_cache.iter().position(|(k, _)| k == &key)
            && let Some((_, old)) = self.mem_cache.remove(pos)
        {
            self.mem_cache_bytes = self.mem_cache_bytes.saturating_sub(old.raw_png.len());
        }
        let add_bytes = entry.raw_png.len();
        self.mem_cache_bytes = self.mem_cache_bytes.saturating_add(add_bytes);
        self.mem_cache.push_back((key, entry));
        while (self.mem_cache.len() > MAX_UG_MEM_CACHE_ENTRIES
            || self.mem_cache_bytes > MAX_UG_MEM_CACHE_BYTES)
            && self.mem_cache.len() > 1
        {
            if let Some((_, evicted)) = self.mem_cache.pop_front() {
                self.mem_cache_bytes = self.mem_cache_bytes.saturating_sub(evicted.raw_png.len());
            } else {
                break;
            }
        }
    }
}

type UgJob = ((i32, i32, i32), PathBuf, FloorOverlayRecord);

struct UgOverlayInner {
    disk_dir: PathBuf,
    timeout: Duration,
    async_fetch: bool,
    offline: bool,
    max_workers: usize,
    floors_by_map: HashMap<i32, Vec<FloorOverlayRecord>>,
    by_floor_id: HashMap<(i32, i32), FloorOverlayRecord>,
    by_group_floor: HashMap<(i32, i32, i32), FloorOverlayRecord>,
    state: Mutex<UgOverlayState>,
    pool: Mutex<Option<mpsc::Sender<UgJob>>>,
}

#[derive(Clone)]
pub struct UndergroundOverlayProvider {
    inner: Arc<UgOverlayInner>,
}

impl std::fmt::Debug for UndergroundOverlayProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UndergroundOverlayProvider")
            .field("disk_dir", &self.inner.disk_dir)
            .field("async_fetch", &self.inner.async_fetch)
            .field("offline", &self.inner.offline)
            .finish()
    }
}

impl UndergroundOverlayProvider {
    pub fn from_config(cfg: UndergroundOverlayConfig) -> CoreResult<Self> {
        std::fs::create_dir_all(&cfg.disk_dir)?;

        let mut floors_by_map = HashMap::new();
        let mut by_floor_id = HashMap::new();
        let mut by_group_floor = HashMap::new();

        for &mid in cfg.maps_db.maps.keys() {
            let records = cfg.maps_db.floor_overlays_for_map(mid);
            for rec in &records {
                by_floor_id.insert((mid, rec.floor_id), rec.clone());
                by_group_floor.insert((mid, rec.group_id, rec.floor_id), rec.clone());
            }
            floors_by_map.insert(mid, records);
        }

        Ok(Self {
            inner: Arc::new(UgOverlayInner {
                disk_dir: cfg.disk_dir,
                timeout: cfg.timeout.max(Duration::from_millis(50)),
                async_fetch: cfg.async_fetch,
                offline: cfg.offline,
                max_workers: cfg.max_workers.max(1),
                floors_by_map,
                by_floor_id,
                by_group_floor,
                state: Mutex::new(UgOverlayState::default()),
                pool: Mutex::new(None),
            }),
        })
    }

    pub fn new(
        disk_dir: Option<PathBuf>,
        maps_db: Option<Arc<MultimapDb>>,
        timeout: Duration,
        async_fetch: bool,
        offline: bool,
        max_workers: usize,
    ) -> CoreResult<Self> {
        let dir = disk_dir.unwrap_or_else(default_ug_overlay_dir);
        let db = maps_db.unwrap_or_else(|| {
            let p = resolve_multimap_json_path(None, None);
            Arc::new(load_multimap_db(&p).unwrap_or_default())
        });
        Self::from_config(UndergroundOverlayConfig {
            disk_dir: dir,
            maps_db: db,
            timeout,
            async_fetch,
            offline,
            max_workers,
        })
    }

    pub fn find_overlapping_floors(
        &self,
        map_id: i32,
        wx: f64,
        wy: f64,
        pad: f64,
    ) -> Vec<FloorOverlayRecord> {
        if !wx.is_finite() || !wy.is_finite() || !pad.is_finite() {
            return Vec::new();
        }
        let Some(list) = self.inner.floors_by_map.get(&map_id) else {
            return Vec::new();
        };
        list.iter()
            .filter(|rec| {
                (rec.lx - pad) <= wx
                    && wx <= (rec.rx + pad)
                    && (rec.ly - pad) <= wy
                    && wy <= (rec.ry + pad)
            })
            .cloned()
            .collect()
    }

    pub fn get_overlay(
        &self,
        map_id: i32,
        floor_id: i32,
        group_id: Option<i32>,
        async_override: Option<bool>,
    ) -> Option<(Arc<FrameBuf>, FloorOverlayRecord)> {
        let entry = self.get_overlay_bytes_entry(map_id, floor_id, group_id, async_override)?;
        let frame = decode_overlay_frame(&entry.raw_png)?;
        Some((Arc::new(frame), entry.record))
    }

    fn get_overlay_bytes_entry(
        &self,
        map_id: i32,
        floor_id: i32,
        group_id: Option<i32>,
        async_override: Option<bool>,
    ) -> Option<CachedOverlayBytes> {
        let rec = group_id
            .and_then(|gid| self.inner.by_group_floor.get(&(map_id, gid, floor_id)))
            .or_else(|| self.inner.by_floor_id.get(&(map_id, floor_id)))?
            .clone();

        let gid = rec.group_id;
        let ckey = (map_id, gid, floor_id);

        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = st.get_cached(&ckey) {
                return Some(cached);
            }
        }

        let local_png = self
            .inner
            .disk_dir
            .join(format!("{map_id}_g{gid}_f{floor_id}.png"));
        if local_png.is_file()
            && let Ok(raw) = std::fs::read(&local_png)
            && has_valid_image_dimensions(&raw)
        {
            let entry = CachedOverlayBytes {
                raw_png: Arc::from(raw),
                record: rec,
            };
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.insert_mem_cache(ckey, entry.clone());
            return Some(entry);
        }

        if self.inner.offline {
            return None;
        }

        let effective_async = async_override.unwrap_or(self.inner.async_fetch);
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return None;
            }
            if let Some(until) = st.net_backoff_until
                && Instant::now() < until
            {
                return None;
            }
            if st.inflight.contains(&ckey) {
                return None;
            }
            st.inflight.insert(ckey);
        }

        if effective_async {
            if self.enqueue_job((ckey, local_png, rec)).is_err() {
                let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&ckey);
            }
            return None;
        }

        let inner = Arc::clone(&self.inner);
        run_blocking_http(move || {
            let http = build_blocking_client(inner.timeout);
            inner.fetch_and_cache_overlay(http.as_ref(), ckey, &local_png, rec)
        })
    }

    pub fn close(&self) {
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
        }
        let mut pool = self.inner.pool.lock().unwrap_or_else(|e| e.into_inner());
        *pool = None;
    }

    fn enqueue_job(&self, job: UgJob) -> Result<(), ()> {
        let mut pool_guard = self.inner.pool.lock().unwrap_or_else(|e| e.into_inner());
        {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return Err(());
            }
        }
        if pool_guard.is_none() {
            let (tx, rx) = mpsc::channel::<UgJob>();
            let rx = Arc::new(Mutex::new(rx));
            for idx in 0..self.inner.max_workers {
                let inner = Arc::clone(&self.inner);
                let rx_clone = Arc::clone(&rx);
                let _ = std::thread::Builder::new()
                    .name(format!("gh-ug-overlay-{idx}"))
                    .spawn(move || {
                        let http = build_blocking_client(inner.timeout);
                        loop {
                            let next = {
                                let guard = rx_clone.lock().unwrap_or_else(|e| e.into_inner());
                                guard.recv()
                            };
                            let Ok((ckey, local_png, rec)) = next else {
                                break;
                            };
                            let is_closed = {
                                let st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                                st.closed
                            };
                            if is_closed {
                                let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                                st.inflight.remove(&ckey);
                                continue;
                            }
                            let _ = inner.fetch_and_cache_overlay(
                                http.as_ref(),
                                ckey,
                                &local_png,
                                rec,
                            );
                        }
                    });
            }
            *pool_guard = Some(tx);
        }
        if let Some(tx) = pool_guard.as_ref() {
            tx.send(job).map_err(|_| ())
        } else {
            Err(())
        }
    }
}

impl Drop for UndergroundOverlayProvider {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.close();
        }
    }
}

impl UndergroundOverlaySource for UndergroundOverlayProvider {
    fn get_overlay_bytes(&self, map_id: i32, floor_id: &str, async_fetch: bool) -> Option<Vec<u8>> {
        let trimmed = floor_id.trim();
        let (group_id, fid) = if let Some((g_str, f_str)) = trimmed.split_once(':') {
            let gid = g_str.trim().parse::<i32>().ok()?;
            let fid = f_str.trim().parse::<i32>().ok()?;
            (Some(gid), fid)
        } else {
            let fid = trimmed.parse::<i32>().ok()?;
            (None, fid)
        };
        self.get_overlay_bytes_entry(map_id, fid, group_id, Some(async_fetch))
            .map(|entry| entry.raw_png.to_vec())
    }
}

impl UgOverlayInner {
    fn record_transient_error(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.consecutive_errors = st.consecutive_errors.saturating_add(1);
        if st.consecutive_errors >= MAX_TRANSIENT_ERRORS {
            st.net_backoff_until =
                Some(Instant::now() + Duration::from_secs_f64(NET_BACKOFF_SECONDS));
        }
    }

    fn record_success(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.consecutive_errors = 0;
        st.net_backoff_until = None;
    }

    fn fetch_and_cache_overlay(
        &self,
        http: Option<&reqwest::blocking::Client>,
        ckey: (i32, i32, i32),
        local_png: &Path,
        rec: FloorOverlayRecord,
    ) -> Option<CachedOverlayBytes> {
        struct InflightGuard<'a> {
            state: &'a Mutex<UgOverlayState>,
            ckey: (i32, i32, i32),
        }
        impl Drop for InflightGuard<'_> {
            fn drop(&mut self) {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&self.ckey);
            }
        }

        let _guard = InflightGuard {
            state: &self.state,
            ckey,
        };

        let client = match http {
            Some(c) => c,
            None => {
                self.record_transient_error();
                return None;
            }
        };

        let resp = match client
            .get(&rec.overlay.url)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .timeout(self.timeout)
            .send()
        {
            Ok(r) => r,
            Err(_) => {
                self.record_transient_error();
                return None;
            }
        };

        if !resp.status().is_success() {
            self.record_transient_error();
            return None;
        }

        let raw = match resp.bytes() {
            Ok(b) => b.to_vec(),
            Err(_) => {
                self.record_transient_error();
                return None;
            }
        };

        if decode_overlay_frame(&raw).is_none() {
            self.record_transient_error();
            return None;
        }

        if atomic_io::write_bytes_atomic(local_png, &raw).is_err() {
            self.record_transient_error();
            return None;
        }

        self.record_success();
        let entry = CachedOverlayBytes {
            raw_png: Arc::from(raw),
            record: rec,
        };
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.insert_mem_cache(ckey, entry.clone());
        Some(entry)
    }
}

pub(crate) fn run_blocking_http<F, T>(f: F) -> Option<T>
where
    F: FnOnce() -> Option<T> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        let handle = std::thread::Builder::new()
            .name("gh-blocking-http".to_owned())
            .spawn(f)
            .ok()?;
        handle.join().ok().flatten()
    } else {
        f()
    }
}

fn build_blocking_client(timeout: Duration) -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .ok()
}

fn decode_tile_gray256(raw: &[u8]) -> Option<FrameBuf> {
    let img = image::load_from_memory(raw).ok()?;
    let gray = img.to_luma8();
    let final_img = if gray.width() != 256 || gray.height() != 256 {
        image::imageops::resize(&gray, 256, 256, FilterType::Triangle)
    } else {
        gray
    };
    Some(FrameBuf {
        width: 256,
        height: 256,
        stride: 256,
        format: PixelFormat::Gray8,
        data: final_img.into_raw(),
    })
}

fn has_valid_image_dimensions(raw: &[u8]) -> bool {
    ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .is_some_and(|(w, h)| w > 0 && h > 0)
}

fn decode_overlay_frame(raw: &[u8]) -> Option<FrameBuf> {
    let img = image::load_from_memory(raw).ok()?;
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return None;
    }
    match img.color() {
        ColorType::L8 | ColorType::L16 => {
            let g = img.to_luma8();
            Some(FrameBuf {
                width: w,
                height: h,
                stride: w as usize,
                format: PixelFormat::Gray8,
                data: g.into_raw(),
            })
        }
        _ => {
            let rgba = img.to_rgba8();
            Some(FrameBuf {
                width: w,
                height: h,
                stride: (w as usize) * 4,
                format: PixelFormat::Rgba8,
                data: rgba.into_raw(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::{Path as AxumPath, State};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use gh_core::multimap::{
        DetailV2Meta, FloorEntry, FloorOverlayMeta, MapMeta, PointGroupEntry,
    };
    use image::{ImageBuffer, ImageFormat, Luma, Rgba, RgbaImage};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    use tokio::net::TcpListener;

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_net_hd_assets_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_test_db(overlay_url: &str) -> Arc<MultimapDb> {
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
                areas: Vec::new(),
                landmark_labels: Vec::new(),
                waypoints: Vec::new(),
                point_groups: vec![PointGroupEntry {
                    id: 100,
                    map_id: 2,
                    name: "Ruins".to_owned(),
                    g_floor_name: "G".to_owned(),
                    default_floor_index: 0,
                    floors: vec![FloorEntry {
                        id: 501,
                        floor_name: "Upper".to_owned(),
                        floor_name_short: "B1".to_owned(),
                        overlay: Some(FloorOverlayMeta {
                            url: overlay_url.to_owned(),
                            l_x: -100.0,
                            l_y: -100.0,
                            r_x: 100.0,
                            r_y: 100.0,
                            ..Default::default()
                        }),
                        index: 0,
                    }],
                }],
            },
        );
        Arc::new(MultimapDb {
            version: "4.0.0".to_owned(),
            maps,
        })
    }

    fn make_webp_tile(val: u8) -> Vec<u8> {
        let img = ImageBuffer::from_pixel(256, 256, Luma([val]));
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut buf, ImageFormat::WebP)
            .unwrap();
        buf.into_inner()
    }

    fn make_gray_png(val: u8) -> Vec<u8> {
        let img = ImageBuffer::from_pixel(256, 256, Luma([val]));
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn make_rgba_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([12, 34, 56, 255]));
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[derive(Clone)]
    struct MockAssetServer {
        tile_hits: Arc<AtomicUsize>,
        overlay_hits: Arc<AtomicUsize>,
        tile_webp: Vec<u8>,
        overlay_png: Vec<u8>,
    }

    async fn spawn_mock_assets(
        state: MockAssetServer,
    ) -> (SocketAddrWrapper, tokio::sync::oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/tiles/{mid}/{ver}/{file}",
                get(
                    |State(st): State<MockAssetServer>,
                     AxumPath((_mid, _ver, file)): AxumPath<(i32, String, String)>| async move {
                        st.tile_hits.fetch_add(1, Ordering::SeqCst);
                        if file.starts_with("9_9_") {
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        if file.starts_with("8_8_") {
                            return StatusCode::FORBIDDEN.into_response();
                        }
                        if file.starts_with("7_") {
                            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                        }
                        ([(axum::http::header::CONTENT_TYPE, "image/webp")], st.tile_webp)
                            .into_response()
                    },
                ),
            )
            .route(
                "/overlay/501.png",
                get(|State(st): State<MockAssetServer>| async move {
                    let count = st.overlay_hits.fetch_add(1, Ordering::SeqCst);
                    if count < 3 {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    ([(axum::http::header::CONTENT_TYPE, "image/png")], st.overlay_png)
                        .into_response()
                }),
            )
            .with_state(state);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        (SocketAddrWrapper(format!("http://{addr}")), tx)
    }

    struct SocketAddrWrapper(String);

    #[test]
    fn verify_v53_offline_disk_cache_and_non_blocking_async_miss() {
        let base = make_temp_dir("v53_offline");
        let tile_dir = base.join("tiles");
        let ug_dir = base.join("ug");
        let db = make_test_db("https://example.invalid/501.png");

        // 1. Offline HD tile provider: miss < 5ms, then disk hit returns 65536-byte Gray8
        let hd_off = HoyolabHdTileProvider::from_config(HdTileConfig {
            disk_dir: tile_dir.clone(),
            maps_db: Arc::clone(&db),
            offline: true,
            ..Default::default()
        })
        .unwrap();

        let t0 = Instant::now();
        assert!(hd_off.request_tile(2, -1, 12, 12, false).is_none());
        assert!(t0.elapsed() < Duration::from_millis(25));

        let (png_path, _) = hd_off.tile_paths(2, -1, 12, 12);
        std::fs::create_dir_all(png_path.parent().unwrap()).unwrap();
        std::fs::write(&png_path, make_gray_png(123)).unwrap();

        let bytes = hd_off
            .request_tile(2, -1, 12, 12, false)
            .expect("disk hit tile");
        assert_eq!(bytes.len(), 256 * 256);
        assert_eq!(bytes[0], 123);
        assert!(hd_off.disk_hits() >= 1);
        hd_off.close();

        // 2. Async non-blocking miss (< 25ms) + invalid coords rejected
        let hd_async = HoyolabHdTileProvider::from_config(HdTileConfig {
            disk_dir: tile_dir.clone(),
            maps_db: Arc::clone(&db),
            tile_base: "http://127.0.0.1:9/tiles".to_owned(),
            timeout: Duration::from_millis(150),
            async_fetch: true,
            offline: false,
            max_workers: 2,
        })
        .unwrap();
        assert!(hd_async.request_tile(2, -3, 0, 0, true).is_none());
        assert!(hd_async.request_tile(2, -1, -1, 0, true).is_none());
        let t1 = Instant::now();
        assert!(hd_async.request_tile(2, -1, 5, 5, true).is_none());
        assert!(t1.elapsed() < Duration::from_millis(25));
        hd_async.close();

        // 3. Offline UndergroundOverlayProvider: overlapping floors + disk load + gid:fid trait
        let ug_off = UndergroundOverlayProvider::from_config(UndergroundOverlayConfig {
            disk_dir: ug_dir.clone(),
            maps_db: Arc::clone(&db),
            offline: true,
            ..Default::default()
        })
        .unwrap();
        let floors = ug_off.find_overlapping_floors(2, 0.0, 0.0, 40.0);
        assert_eq!(floors.len(), 1);
        assert_eq!(floors[0].floor_id, 501);

        let disk_png = ug_dir.join("2_g100_f501.png");
        let raw_overlay = make_rgba_png(64, 32);
        std::fs::write(&disk_png, &raw_overlay).unwrap();

        let (frame, rec) = ug_off
            .get_overlay(2, 501, Some(100), Some(false))
            .expect("disk overlay");
        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 32);
        assert_eq!(frame.format, PixelFormat::Rgba8);
        assert_eq!(rec.group_id, 100);

        let trait_bytes = ug_off
            .get_overlay_bytes(2, "100:501", false)
            .expect("trait bytes");
        assert_eq!(trait_bytes, raw_overlay);
        ug_off.close();

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_tile_fetch_404_marker_circuit_breaker_and_prefetch_region() {
        let tile_hits = Arc::new(AtomicUsize::new(0));
        let overlay_hits = Arc::new(AtomicUsize::new(0));
        let srv = MockAssetServer {
            tile_hits: Arc::clone(&tile_hits),
            overlay_hits: Arc::clone(&overlay_hits),
            tile_webp: make_webp_tile(177),
            overlay_png: make_rgba_png(48, 48),
        };
        let (base_url, shutdown_tx) = spawn_mock_assets(srv).await;

        let base = make_temp_dir("online");
        let db = make_test_db(&format!("{}/overlay/501.png", base_url.0));

        // Note: calling sync Trait methods from inside #[tokio::test] explicitly validates Tokio-safety!
        let hd = HoyolabHdTileProvider::from_config(HdTileConfig {
            disk_dir: base.join("tiles"),
            maps_db: Arc::clone(&db),
            tile_base: format!("{}/tiles", base_url.0),
            timeout: Duration::from_secs(2),
            async_fetch: true,
            offline: false,
            max_workers: 2,
        })
        .unwrap();

        // 404 & 403 create .404 markers and do NOT trip transient error breaker
        assert!(hd.request_tile(2, -1, 9, 9, false).is_none());
        assert!(hd.request_tile(2, -1, 8, 8, false).is_none());
        let (_, miss_404) = hd.tile_paths(2, -1, 9, 9);
        let (_, miss_403) = hd.tile_paths(2, -1, 8, 8);
        assert!(miss_404.is_file());
        assert!(miss_403.is_file());

        let hits_before_repeat = tile_hits.load(Ordering::SeqCst);
        assert!(hd.request_tile(2, -1, 9, 9, false).is_none());
        assert_eq!(tile_hits.load(Ordering::SeqCst), hits_before_repeat);

        // 3 consecutive 500 errors trip 60s circuit breaker
        assert!(hd.request_tile(2, -1, 7, 1, false).is_none());
        assert!(hd.request_tile(2, -1, 7, 2, false).is_none());
        assert!(hd.request_tile(2, -1, 7, 3, false).is_none());
        let hits_tripped = tile_hits.load(Ordering::SeqCst);
        assert!(hd.request_tile(2, -1, 1, 1, false).is_none());
        assert_eq!(
            tile_hits.load(Ordering::SeqCst),
            hits_tripped,
            "circuit breaker must short-circuit requests"
        );

        // Reset breaker & test async fetch -> disk cache + prefetch_region
        hd.inner.record_success();
        assert!(hd.request_tile(2, -1, 2, 2, true).is_none());
        let mut loaded_async = None;
        let async_dl = Instant::now() + Duration::from_secs(15);
        while Instant::now() < async_dl {
            if let Some(t) = hd.request_tile(2, -1, 2, 2, false) {
                loaded_async = Some(t);
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(loaded_async.unwrap()[0], 177);
        assert!(hd.downloads() >= 1);

        // prefetch_region refuses > 400 tiles
        let huge_err = hd
            .prefetch_region(2, [-8000.0, 8000.0, -8000.0, 8000.0], 0, 2)
            .expect_err("must reject huge prefetch");
        assert!(matches!(huge_err, CoreError::InvalidArgument(_)));

        let count = hd
            .prefetch_region(2, [-8192.0, -7700.0, -8192.0, -7700.0], -1, 2)
            .expect("small prefetch");
        assert!(count >= 1);
        hd.close();

        // UndergroundOverlayProvider: 3 errors trip breaker, zero .404 files written
        let ug = UndergroundOverlayProvider::from_config(UndergroundOverlayConfig {
            disk_dir: base.join("ug"),
            maps_db: Arc::clone(&db),
            timeout: Duration::from_secs(2),
            async_fetch: false,
            offline: false,
            max_workers: 1,
        })
        .unwrap();
        assert!(ug.get_overlay_bytes(2, "501", false).is_none());
        assert!(ug.get_overlay_bytes(2, "501", false).is_none());
        assert!(ug.get_overlay_bytes(2, "501", false).is_none());
        assert!(!base.join("ug").join("2_g100_f501.404").exists());
        let ug_hits_tripped = overlay_hits.load(Ordering::SeqCst);
        assert!(ug.get_overlay_bytes(2, "501", false).is_none());
        assert_eq!(overlay_hits.load(Ordering::SeqCst), ug_hits_tripped);

        ug.inner.record_success();
        let png = ug.get_overlay_bytes(2, "501", false).expect("overlay ok");
        assert!(png.starts_with(b"\x89PNG"));
        assert!(base.join("ug").join("2_g100_f501.png").is_file());
        ug.close();

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn underground_overlay_mem_cache_is_lru_bounded_by_count_and_bytes() {
        let base = make_temp_dir("ug_lru");
        let ug_dir = base.join("ug");
        std::fs::create_dir_all(&ug_dir).unwrap();

        let floors: Vec<FloorEntry> = (1..=12i32)
            .map(|fid| FloorEntry {
                id: fid,
                floor_name: format!("F{fid}"),
                floor_name_short: format!("B{fid}"),
                overlay: Some(FloorOverlayMeta {
                    url: format!("https://example.invalid/{fid}.png"),
                    l_x: -10.0,
                    l_y: -10.0,
                    r_x: 10.0,
                    r_y: 10.0,
                    ..Default::default()
                }),
                index: fid,
            })
            .collect();

        let mut maps = BTreeMap::new();
        maps.insert(
            2,
            MapMeta {
                id: 2,
                name: "Teyvat".to_owned(),
                point_groups: vec![PointGroupEntry {
                    id: 10,
                    map_id: 2,
                    name: "Cave".to_owned(),
                    g_floor_name: "G".to_owned(),
                    default_floor_index: 0,
                    floors,
                }],
                ..Default::default()
            },
        );
        let db = Arc::new(MultimapDb {
            version: "4.0.0".to_owned(),
            maps,
        });

        let small_png = make_rgba_png(16, 16);
        for fid in 1..=10 {
            std::fs::write(ug_dir.join(format!("2_g10_f{fid}.png")), &small_png).unwrap();
        }

        let ug = UndergroundOverlayProvider::from_config(UndergroundOverlayConfig {
            disk_dir: ug_dir.clone(),
            maps_db: db,
            offline: true,
            ..Default::default()
        })
        .unwrap();

        for fid in 1..=10 {
            assert!(ug.get_overlay_bytes(2, &format!("10:{fid}"), false).is_some());
        }
        {
            let st = ug.inner.state.lock().unwrap();
            assert_eq!(st.mem_cache.len(), MAX_UG_MEM_CACHE_ENTRIES);
            assert!(st.mem_cache.iter().all(|(( _, _, fid), _)| *fid >= 3));
        }

        // Byte bound check: insert two > 6 MiB raw payloads into state directly and ensure eviction
        {
            let mut st = ug.inner.state.lock().unwrap();
            let big = CachedOverlayBytes {
                raw_png: Arc::from(vec![0u8; 7 * 1024 * 1024]),
                record: FloorOverlayRecord {
                    map_id: 2,
                    group_id: 10,
                    floor_index: 0,
                    floor_id: 11,
                    floor_name: "F11".to_owned(),
                    floor_short: "B11".to_owned(),
                    lx: -10.0,
                    rx: 10.0,
                    ly: -10.0,
                    ry: 10.0,
                    overlay: FloorOverlayMeta::default(),
                },
            };
            st.insert_mem_cache((2, 10, 11), big.clone());
            assert!(st.mem_cache_bytes <= MAX_UG_MEM_CACHE_BYTES);
            st.insert_mem_cache((2, 10, 12), big);
            assert!(st.mem_cache_bytes <= MAX_UG_MEM_CACHE_BYTES);
            assert_eq!(st.mem_cache.len(), 1);
        }

        ug.close();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unified_webp_cache_serves_both_cv_gray_and_radar_color_plus_legacy_png_fallback() {
        let tile_hits = Arc::new(AtomicUsize::new(0));
        let overlay_hits = Arc::new(AtomicUsize::new(0));
        let color_img = RgbaImage::from_pixel(256, 256, Rgba([220, 60, 30, 255]));
        let mut color_buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(color_img)
            .write_to(&mut color_buf, ImageFormat::WebP)
            .unwrap();
        let srv = MockAssetServer {
            tile_hits: Arc::clone(&tile_hits),
            overlay_hits,
            tile_webp: color_buf.into_inner(),
            overlay_png: make_rgba_png(16, 16),
        };
        let (base_url, shutdown_tx) = spawn_mock_assets(srv).await;
        let base = make_temp_dir("unified_webp");
        let db = make_test_db(&format!("{}/overlay/501.png", base_url.0));

        let hd = HoyolabHdTileProvider::from_config(HdTileConfig {
            disk_dir: base.join("tiles"),
            maps_db: Arc::clone(&db),
            tile_base: format!("{}/tiles", base_url.0),
            timeout: Duration::from_secs(2),
            async_fetch: false,
            offline: false,
            max_workers: 1,
        })
        .unwrap();

        // 1. CV fetches tile once -> stored as full-color `.webp`.
        let gray = hd.request_tile(2, -1, 3, 4, false).expect("cv gray tile");
        assert_eq!(gray.len(), 256 * 256);
        assert!(hd.color_tile_path(2, -1, 3, 4).is_file());
        assert_eq!(tile_hits.load(Ordering::SeqCst), 1);

        // 2. Radar reads the exact same tile in full color with 0 extra HTTP hits!
        let color_bytes = hd
            .get_color_tile_bytes(2, -1, 3, 4, Some(false))
            .expect("radar color bytes");
        assert_eq!(tile_hits.load(Ordering::SeqCst), 1);
        let decoded = image::load_from_memory(&color_bytes).unwrap().to_rgba8();
        let px = decoded.get_pixel(10, 10);
        assert!(px[0] > 180 && px[2] < 80, "color channels must be preserved: {px:?}");

        // 3. Coarse min_zoom (-4) is served cleanly for Radar even though CV is bounded to -2..=0.
        let coarse = hd
            .get_color_tile_bytes(2, -4, 0, 0, Some(false))
            .expect("coarse min_zoom tile");
        assert!(coarse.len() >= 12);

        // 4. Legacy `.png` already on disk still works for CV.
        let (legacy_png, _) = hd.tile_paths(2, -1, 6, 6);
        std::fs::create_dir_all(legacy_png.parent().unwrap()).unwrap();
        std::fs::write(&legacy_png, make_gray_png(88)).unwrap();
        let legacy_gray = hd.request_tile(2, -1, 6, 6, false).expect("legacy png");
        assert_eq!(legacy_gray[0], 88);

        hd.close();
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&base);
    }
}
