//! Shared CV/tracking stack assembly used by both production `GenshinHelper.exe` (`main.rs`)
//! and dev-only deterministic replay (`gh-sim`).
//!
//! Keeping one canonical builder prevents drift between live and replay configurations; every
//! intentional difference (offline cache, synchronous tile/relocator warmup, explicit `data_dir`,
//! OCR provider) is passed explicitly via [`TrackingStackOptions`].

use gh_core::CoreResult;
use gh_core::multimap::MultimapDb;
use gh_core::paths::AppPaths;
use gh_core::traits::{OcrProvider, TileProvider, UndergroundOverlaySource};
use gh_cv::global_relocator::TeyvatGlobalRelocator;
use gh_cv::locator::{LocatorPaths, MapLocator};
use gh_cv::map_state::MapScreenStateMachine;
use gh_cv::minimap_tracker::{HybridMinimapTracker, MinimapTrackerConfig};
use gh_cv::tile_cache::{LruTerrainTileCache, TileCacheConfig};
use gh_net::hd_assets::{
    HdTileConfig, HoyolabHdTileProvider, TILE_BASE, UndergroundOverlayConfig,
    UndergroundOverlayProvider,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const PRODUCTION_TILE_CACHE_CAPACITY: usize = 72;

pub struct TrackingStackOptions {
    /// Explicit dataset directory (`multimap_v4.json`, `terrain_n2_v4.npz`, `cave_masks_v4.npz`,
    /// optional `ui_anchors_v4.npz`). `None` uses `MapLocator::new` / `MapScreenStateMachine::new`
    /// default runtime resolution.
    pub explicit_data_dir: Option<PathBuf>,
    pub tiles_dir: PathBuf,
    pub ug_overlays_dir: PathBuf,
    pub paimon_cache_path: PathBuf,
    /// Optional explicit path for `teyvat_global_index_v3.npz`. `None` uses `AppPaths` default.
    pub global_index_path: Option<PathBuf>,
    pub offline: bool,
    pub async_fetch: bool,
    /// When `true`, blocks until `TeyvatGlobalRelocator::ensure_ready()` completes (replay).
    /// When `false`, kicks off `warmup_async()` in the background (live app startup).
    pub sync_relocator_warmup: bool,
    pub ocr: Option<Arc<dyn OcrProvider>>,
}

impl TrackingStackOptions {
    pub fn for_live(paths: &AppPaths, offline_mode: bool, ocr: Option<Arc<dyn OcrProvider>>) -> Self {
        Self {
            explicit_data_dir: None,
            tiles_dir: paths.tiles_dir.clone(),
            ug_overlays_dir: paths.ug_overlays_dir.clone(),
            paimon_cache_path: paths.paimon_cache_path(),
            global_index_path: None,
            offline: offline_mode,
            async_fetch: true,
            sync_relocator_warmup: false,
            ocr,
        }
    }
}

pub struct TrackingStack {
    pub locator: Arc<MapLocator>,
    pub maps_db: Arc<MultimapDb>,
    pub hd_provider: Arc<HoyolabHdTileProvider>,
    pub ug_provider: Arc<UndergroundOverlayProvider>,
    pub tile_cache: Arc<LruTerrainTileCache>,
    pub global_relocator: Arc<TeyvatGlobalRelocator>,
    pub minimap_tracker: HybridMinimapTracker,
    pub state_machine: MapScreenStateMachine,
    pub relocator_ready: bool,
    /// Dataset directory actually loaded by this tracking stack.
    pub data_dir: PathBuf,
    /// Non-path label identifying the dataset source (`"installed"`, `"dev"`, `"bundle"`, `"env_override"`, `"custom"`).
    pub data_source: String,
}

fn classify_data_source(paths: &AppPaths, dir: &Path) -> String {
    if paths.dev_data_dir.as_deref() == Some(dir) {
        return "dev".to_owned();
    }
    if std::env::var_os("GENSHIN_HELPER_DATA_DIR")
        .is_some_and(|v| !v.to_string_lossy().trim().is_empty())
        && dir == paths.data_dir
    {
        return "env_override".to_owned();
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
        && exe_dir.join("data") == dir
    {
        return "bundle".to_owned();
    }
    "installed".to_owned()
}

fn resolve_live_data_dir_and_source() -> (PathBuf, String) {
    let paths = AppPaths::resolve();
    let mm = gh_core::assets::resolve_data_asset(gh_core::assets::REQUIRED_ASSETS[0]);
    let dir = if mm.is_file() {
        mm.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths.data_dir.clone())
    } else {
        gh_core::assets::resolve_data_dir()
    };
    let source = classify_data_source(&paths, &dir);
    (dir, source)
}

pub fn build_tracking_stack(opts: TrackingStackOptions) -> CoreResult<TrackingStack> {
    let (data_dir, data_source) = match opts.explicit_data_dir {
        Some(ref dir) => (dir.clone(), "custom".to_owned()),
        None => resolve_live_data_dir_and_source(),
    };
    let (locator, state_machine, npz_path, use_default_npz) = match opts.explicit_data_dir {
        Some(ref dir) => {
            let loc = Arc::new(MapLocator::with_paths(
                LocatorPaths {
                    db_path: Some(dir.join("multimap_v4.json")),
                    terrain_npz_path: Some(dir.join("terrain_n2_v4.npz")),
                    cave_masks_npz_path: Some(dir.join("cave_masks_v4.npz")),
                    ui_anchors_npz_path: Some(dir.join("ui_anchors_v4.npz")),
                    legacy_underground_db_path: Some(dir.join("underground_db.json")),
                },
                opts.ocr,
            ));
            let anchors = dir.join("ui_anchors_v4.npz");
            let sm = if anchors.is_file() {
                MapScreenStateMachine::with_anchors_path(Some(&anchors))
            } else {
                MapScreenStateMachine::with_anchors_path(None)
            };
            (loc, sm, Some(dir.join("terrain_n2_v4.npz")), false)
        }
        None => (
            Arc::new(MapLocator::new(opts.ocr)),
            MapScreenStateMachine::new(),
            None,
            true,
        ),
    };

    let maps_db = locator.multimap_db_arc();

    let hd_provider = Arc::new(HoyolabHdTileProvider::from_config(HdTileConfig {
        disk_dir: opts.tiles_dir.clone(),
        maps_db: Arc::clone(&maps_db),
        tile_base: TILE_BASE.to_owned(),
        timeout: if opts.offline {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(5)
        },
        async_fetch: opts.async_fetch,
        offline: opts.offline,
        max_workers: if opts.offline { 1 } else { 4 },
    })?);

    let ug_provider = Arc::new(UndergroundOverlayProvider::from_config(
        UndergroundOverlayConfig {
            disk_dir: opts.ug_overlays_dir,
            maps_db: Arc::clone(&maps_db),
            timeout: if opts.offline {
                Duration::from_secs(1)
            } else {
                Duration::from_secs(8)
            },
            async_fetch: opts.async_fetch,
            offline: opts.offline,
            max_workers: if opts.offline { 1 } else { 2 },
        },
    )?);

    let tile_cache = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: PRODUCTION_TILE_CACHE_CAPACITY,
            npz_path,
            use_default_npz,
            disk_cache_dir: Some(opts.tiles_dir),
            terrain_data: locator.terrain_data().clone(),
            tile_provider: Some(Arc::clone(&hd_provider) as Arc<dyn TileProvider>),
            async_fetch: opts.async_fetch,
            ..Default::default()
        })
        .map_err(|e| {
            gh_core::CoreError::InvalidData(format!("LruTerrainTileCache init failed: {e}"))
        })?,
    );

    let global_relocator = Arc::new(TeyvatGlobalRelocator::from_terrain_data_with_fallback(
        locator.terrain_data(),
        opts.global_index_path,
        !opts.sync_relocator_warmup,
        use_default_npz,
    ));
    let relocator_ready = if opts.sync_relocator_warmup {
        global_relocator.ensure_ready()
    } else {
        global_relocator.warmup_async();
        global_relocator.is_ready()
    };

    let minimap_tracker = HybridMinimapTracker::new(
        Arc::clone(&tile_cache),
        MinimapTrackerConfig::production_defaults(),
    )
    .map_err(|e| {
        gh_core::CoreError::InvalidData(format!("HybridMinimapTracker init failed: {e}"))
    })?
    .with_multimap_db(Arc::clone(&maps_db))
    .with_ug_provider(Arc::clone(&ug_provider) as Arc<dyn UndergroundOverlaySource>)
    .with_global_relocator(Arc::clone(&global_relocator))
    .with_paimon_cache_path(opts.paimon_cache_path);

    Ok(TrackingStack {
        locator,
        maps_db,
        hd_provider,
        ug_provider,
        tile_cache,
        global_relocator,
        minimap_tracker,
        state_machine,
        relocator_ready,
        data_dir,
        data_source,
    })
}
