//! In-process HoYoLAB public API dataset hydration implementing `gh_core::traits::DatasetSyncRunner`
//! (`src/hoyolab_sync.py`).

use gh_core::assets::{
    MANIFEST_FILENAME, OPTIONAL_ASSETS, REQUIRED_ASSETS, build_manifest, promote_staged_assets,
    validate_asset_file, verify_data_dir, write_manifest,
};
use gh_core::atomic_io;
use gh_core::config::HoyolabSyncConfig;
use gh_core::multimap::{
    AreaEntry, DEFAULT_MULTIMAP_VERSION, DetailV2Meta, FloorEntry, FloorOverlayMeta,
    LandmarkLabelEntry, MapMeta, MultimapDb, PointGroupEntry, WaypointEntry,
    WaypointPointGroupRef,
};
use gh_core::npz::{NpyArray, NpzArchive};
use gh_core::paths::AppPaths;
use gh_core::traits::{DatasetSyncRunner, MapVersionCheckResult};
use gh_core::{CoreError, CoreResult};
use image::imageops::FilterType;
use image::{ImageBuffer, Luma};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const HEADER_RPC_MAP_VERSION: HeaderName = HeaderName::from_static("x-rpc-map_version");
const DISCOVERY_CONCURRENCY: usize = 12;
const ASSET_CONCURRENCY: usize = 16;

static UPDATE_STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredMap {
    pub id: i32,
    pub name: String,
    pub priority: i32,
    pub detail_v2: DetailV2Meta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupUpdateOutcome {
    Disabled,
    UpToDate(MapVersionCheckResult),
    UpdateAvailable(MapVersionCheckResult),
    Updated(MapVersionCheckResult),
}

#[derive(Debug, Clone)]
pub struct HoyolabSyncClient {
    api_timeout: Duration,
    list_timeout: Duration,
    bytes_timeout: Duration,
}

pub type HoyolabSyncRunner = HoyolabSyncClient;

impl Default for HoyolabSyncClient {
    fn default() -> Self {
        Self {
            api_timeout: Duration::from_secs(15),
            list_timeout: Duration::from_secs(20),
            bytes_timeout: Duration::from_secs(12),
        }
    }
}

impl HoyolabSyncClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeouts(
        api_timeout: Duration,
        list_timeout: Duration,
        bytes_timeout: Duration,
    ) -> Self {
        Self {
            api_timeout: api_timeout.max(Duration::from_millis(100)),
            list_timeout: list_timeout.max(Duration::from_millis(100)),
            bytes_timeout: bytes_timeout.max(Duration::from_millis(100)),
        }
    }

    pub fn discover_active_maps(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        extra_ids: &[i32],
    ) -> CoreResult<BTreeMap<i32, DiscoveredMap>> {
        let client = self.clone();
        let cfg = sync_cfg.clone();
        let extras = extra_ids.to_vec();
        run_on_worker_runtime(move || async move {
            client.discover_active_maps_async(&cfg, &extras).await
        })
    }

    pub fn fetch_map_bundle(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        meta: &DiscoveredMap,
        legacy_shorts: &BTreeMap<(i32, i32), String>,
    ) -> CoreResult<MapMeta> {
        let client = self.clone();
        let cfg = sync_cfg.clone();
        let m = meta.clone();
        let shorts = legacy_shorts.clone();
        run_on_worker_runtime(move || async move {
            let http = build_http_client()?;
            client.fetch_map_bundle_async(&http, &cfg, &m, &shorts).await
        })
    }

    pub fn download_cave_masks(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        maps: &BTreeMap<i32, MapMeta>,
        out_npz: &Path,
    ) -> CoreResult<usize> {
        let client = self.clone();
        let cfg = sync_cfg.clone();
        let maps_owned = maps.clone();
        let out_path = out_npz.to_path_buf();
        run_on_worker_runtime(move || async move {
            let http = build_http_client()?;
            client
                .download_cave_masks_async(&http, &cfg, &maps_owned, &out_path)
                .await
        })
    }

    pub fn download_terrain_tiles(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        maps: &BTreeMap<i32, MapMeta>,
        out_npz: &Path,
    ) -> CoreResult<BTreeMap<i32, (usize, usize)>> {
        let client = self.clone();
        let cfg = sync_cfg.clone();
        let maps_owned = maps.clone();
        let out_path = out_npz.to_path_buf();
        run_on_worker_runtime(move || async move {
            let http = build_http_client()?;
            client
                .download_terrain_tiles_async(&http, &cfg, &maps_owned, &out_path)
                .await
        })
    }

    pub fn sync_to_directory(&self, data_dir: &Path) -> CoreResult<PathBuf> {
        self.sync_to_directory_with_cfg(data_dir, &HoyolabSyncConfig::default(), None, None)
    }

    pub fn sync_to_directory_with_cfg(
        &self,
        data_dir: &Path,
        sync_cfg: &HoyolabSyncConfig,
        legacy_ug_path: Option<&Path>,
        progress_cb: Option<&(dyn Fn(&str) + Send + Sync)>,
    ) -> CoreResult<PathBuf> {
        std::fs::create_dir_all(data_dir)?;
        let legacy_shorts = legacy_ug_path
            .map(load_legacy_floor_shorts)
            .unwrap_or_default();

        if let Some(cb) = progress_cb {
            cb("Discovering active HoYoLAB maps...");
        }
        let active_maps = self.discover_active_maps(sync_cfg, &[])?;
        if active_maps.is_empty() {
            return Err(CoreError::Sync(
                "Failed to reach HoYoLAB map API (no active maps discovered).".to_owned(),
            ));
        }

        let client = self.clone();
        let cfg = sync_cfg.clone();
        let out_dir = data_dir.to_path_buf();

        let (maps_bundle, _mask_count, _shapes) = run_on_worker_runtime(move || async move {
            let http = build_http_client()?;
            let mut maps_bundle = BTreeMap::new();
            for (&mid, meta) in &active_maps {
                let bundle = client
                    .fetch_map_bundle_async(&http, &cfg, meta, &legacy_shorts)
                    .await?;
                maps_bundle.insert(mid, bundle);
            }

            let db = MultimapDb {
                version: DEFAULT_MULTIMAP_VERSION.to_owned(),
                maps: maps_bundle.clone(),
            };
            let json_text = db.to_json_string_pretty()?;
            atomic_io::write_str_atomic(&out_dir.join("multimap_v4.json"), &json_text)?;

            let mask_count = client
                .download_cave_masks_async(
                    &http,
                    &cfg,
                    &maps_bundle,
                    &out_dir.join("cave_masks_v4.npz"),
                )
                .await?;
            let shapes = client
                .download_terrain_tiles_async(
                    &http,
                    &cfg,
                    &maps_bundle,
                    &out_dir.join("terrain_n2_v4.npz"),
                )
                .await?;

            Ok((maps_bundle, mask_count, shapes))
        })?;

        if let Some(cb) = progress_cb {
            cb(&format!("Synced {} HoYoLAB maps.", maps_bundle.len()));
        }
        Ok(data_dir.to_path_buf())
    }

    async fn discover_active_maps_async(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        extra_ids: &[i32],
    ) -> CoreResult<BTreeMap<i32, DiscoveredMap>> {
        let http = build_http_client()?;
        let headers = Arc::new(build_api_headers(sync_cfg)?);
        let base_api = sync_cfg.base_api.trim().trim_end_matches('/').to_owned();
        let info_timeout = self.api_timeout.min(Duration::from_secs(10));

        let mut seen = HashSet::new();
        let mut probe_ids = Vec::new();
        for &mid in &sync_cfg.default_map_ids {
            if seen.insert(mid) {
                probe_ids.push(mid);
            }
        }
        for &mid in extra_ids {
            if seen.insert(mid) {
                probe_ids.push(mid);
            }
        }
        let lo = sync_cfg.probe_id_range[0];
        let hi = sync_cfg.probe_id_range[1];
        for mid in lo..=hi {
            if seen.insert(mid) {
                probe_ids.push(mid);
            }
        }

        let sem = Arc::new(Semaphore::new(DISCOVERY_CONCURRENCY));
        let mut join_set = JoinSet::new();

        for mid in probe_ids {
            let permit = Arc::clone(&sem);
            let http = http.clone();
            let headers = Arc::clone(&headers);
            let url = format!("{base_api}/v1/map/info?map_id={mid}&app_sn=ys_obc&lang=vi-vn");
            let priority = sync_cfg.priority_for_map(mid);
            join_set.spawn(async move {
                let Ok(_guard) = permit.acquire_owned().await else {
                    return None;
                };
                let data = fetch_json_opt(&http, &url, &headers, info_timeout).await?;
                if !is_retcode_ok(&data) {
                    return None;
                }
                let info = data.get("data")?.get("info")?.as_object()?;
                let raw_d2 = info.get("detail_v2")?;
                let d2 = parse_detail_v2(raw_d2)?;
                if d2.map_version.trim().is_empty() {
                    return None;
                }
                let name = info
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Map {mid}"));
                Some(DiscoveredMap {
                    id: mid,
                    name,
                    priority,
                    detail_v2: d2,
                })
            });
        }

        let mut out = BTreeMap::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok(Some(map_meta)) = res {
                out.insert(map_meta.id, map_meta);
            }
        }
        Ok(out)
    }

    async fn fetch_map_bundle_async(
        &self,
        http: &reqwest::Client,
        sync_cfg: &HoyolabSyncConfig,
        meta: &DiscoveredMap,
        legacy_shorts: &BTreeMap<(i32, i32), String>,
    ) -> CoreResult<MapMeta> {
        let mid = meta.id;
        let headers = build_api_headers(sync_cfg)?;
        let base_api = sync_cfg.base_api.trim().trim_end_matches('/');

        let areas_url =
            format!("{base_api}/v1/map/get_area_pageLabel?map_id={mid}&app_sn=ys_obc&lang=vi-vn");
        let tree_url = format!("{base_api}/v1/map/label/tree?map_id={mid}&app_sn=ys_obc&lang=vi-vn");
        let pts_url = format!("{base_api}/v2/map/point/list?map_id={mid}&app_sn=ys_obc&lang=vi-vn");
        let pg_url = format!("{base_api}/v2/map/point_group?map_id={mid}&app_sn=ys_obc&lang=vi-vn");

        let (areas_resp, tree_resp, pts_resp, pg_resp) = tokio::join!(
            fetch_json_opt(http, &areas_url, &headers, self.api_timeout),
            fetch_json_opt(http, &tree_url, &headers, self.api_timeout),
            fetch_json_opt(http, &pts_url, &headers, self.list_timeout),
            fetch_json_opt(http, &pg_url, &headers, self.api_timeout),
        );

        let mut areas = Vec::new();
        if let Some(arr) = areas_resp
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.get("list"))
            .and_then(Value::as_array)
        {
            for item in arr {
                let Some(id) = parse_i32_tolerant(item.get("id")) else {
                    continue;
                };
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                areas.push(AreaEntry { id, name });
            }
        }

        let primary_set: BTreeSet<i32> = sync_cfg.primary_landmark_labels.iter().copied().collect();
        let mut landmark_labels = Vec::new();
        if let Some(tree) = tree_resp
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.get("tree"))
            .and_then(Value::as_array)
        {
            let cat1 = tree
                .iter()
                .find(|node| parse_i32_tolerant(node.get("id")) == Some(1))
                .or_else(|| tree.first());
            if let Some(children) = cat1.and_then(|c| c.get("children")).and_then(Value::as_array)
            {
                for ch in children {
                    let Some(lid) = parse_i32_tolerant(ch.get("id")) else {
                        continue;
                    };
                    let name = ch
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    landmark_labels.push(LandmarkLabelEntry {
                        id: lid,
                        name,
                        is_primary: primary_set.contains(&lid),
                    });
                }
            }
        }

        let cat1_id_set: BTreeSet<i32> = if landmark_labels.is_empty() {
            primary_set.clone()
        } else {
            landmark_labels.iter().map(|lbl| lbl.id).collect()
        };

        let mut waypoints = Vec::new();
        if let Some(raw_pts) = pts_resp
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.get("point_list"))
            .and_then(Value::as_array)
        {
            for p in raw_pts {
                let lid = parse_i32_tolerant(p.get("label_id")).unwrap_or(0);
                if !cat1_id_set.contains(&lid) {
                    continue;
                }
                let Some(id) = parse_i64_tolerant(p.get("id")) else {
                    continue;
                };
                let (Some(x_pos), Some(y_pos)) = (
                    parse_f64_tolerant(p.get("x_pos")),
                    parse_f64_tolerant(p.get("y_pos")),
                ) else {
                    continue;
                };
                let pg_clean = p.get("point_group").and_then(Value::as_object).and_then(|pg| {
                    let gid = parse_i32_tolerant(pg.get("group_id"))?;
                    let fid = parse_i32_tolerant(pg.get("floor_id"));
                    Some(WaypointPointGroupRef {
                        group_id: gid,
                        floor_id: fid,
                    })
                });
                waypoints.push(WaypointEntry {
                    id,
                    label_id: lid,
                    is_primary: primary_set.contains(&lid),
                    x_pos,
                    y_pos,
                    area_id: parse_i32_tolerant(p.get("area_id")).unwrap_or(0),
                    map_id: mid,
                    z_level: parse_i32_tolerant(p.get("z_level")).unwrap_or(0),
                    icon_sign: parse_i32_tolerant(p.get("icon_sign")).unwrap_or(0),
                    point_group: pg_clean,
                });
            }
        }

        let mut point_groups = Vec::new();
        if let Some(raw_pgs) = pg_resp
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.get("list"))
            .and_then(Value::as_array)
        {
            for g in raw_pgs {
                let Some(gid) = parse_i32_tolerant(g.get("id")) else {
                    continue;
                };
                let mut floors_out = Vec::new();
                if let Some(floors) = g.get("floors").and_then(Value::as_array) {
                    for (f_idx, fl) in floors.iter().enumerate() {
                        let Some(fid) = parse_i32_tolerant(fl.get("id")) else {
                            continue;
                        };
                        let mut fs = fl
                            .get("floor_name_short")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_ascii_uppercase();
                        if fs.is_empty()
                            && let Some(legacy) = legacy_shorts.get(&(gid, fid))
                        {
                            fs = legacy.clone();
                        }

                        let ov_clean = fl.get("overlay").and_then(Value::as_object).and_then(|ov| {
                            let url = ov
                                .get("url")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())?
                                .to_owned();
                            let lx = parse_f64_tolerant(ov.get("l_x"))?;
                            let rx = parse_f64_tolerant(ov.get("r_x"))?;
                            let ly = parse_f64_tolerant(ov.get("l_y"))?;
                            let ry = parse_f64_tolerant(ov.get("r_y"))?;
                            let width = parse_f64_tolerant(ov.get("width")).unwrap_or((rx - lx).abs());
                            let height =
                                parse_f64_tolerant(ov.get("height")).unwrap_or((ry - ly).abs());
                            Some(FloorOverlayMeta {
                                url,
                                l_x: lx,
                                l_y: ly,
                                r_x: rx,
                                r_y: ry,
                                min_lng: Some(lx.min(rx)),
                                max_lng: Some(lx.max(rx)),
                                min_lat: Some(ly.min(ry)),
                                max_lat: Some(ly.max(ry)),
                                width: Some(width),
                                height: Some(height),
                                reflect_y: None,
                            })
                        });

                        floors_out.push(FloorEntry {
                            id: fid,
                            floor_name: fl
                                .get("floor_name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            floor_name_short: fs,
                            overlay: ov_clean,
                            index: f_idx as i32,
                        });
                    }
                }

                let g_floor_name = g
                    .get("g_floor_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("G")
                    .to_owned();

                point_groups.push(PointGroupEntry {
                    id: gid,
                    map_id: mid,
                    name: g
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    g_floor_name,
                    default_floor_index: parse_i32_tolerant(g.get("default_floor_index"))
                        .unwrap_or(0),
                    floors: floors_out,
                });
            }
        }

        Ok(MapMeta {
            id: mid,
            name: meta.name.clone(),
            priority: meta.priority,
            detail_v2: Some(meta.detail_v2.clone()),
            areas,
            landmark_labels,
            waypoints,
            point_groups,
        })
    }

    async fn download_cave_masks_async(
        &self,
        http: &reqwest::Client,
        sync_cfg: &HoyolabSyncConfig,
        maps: &BTreeMap<i32, MapMeta>,
        out_npz: &Path,
    ) -> CoreResult<usize> {
        let headers = Arc::new(build_bytes_headers(sync_cfg)?);
        let mut tasks = Vec::new();
        for (&mid, mdata) in maps {
            for g in &mdata.point_groups {
                for fl in &g.floors {
                    if let Some(ov) = &fl.overlay {
                        let url = ov.url.trim();
                        if !url.is_empty() {
                            tasks.push((format!("{mid}_{}_{}", g.id, fl.id), url.to_owned()));
                        }
                    }
                }
            }
        }

        let sem = Arc::new(Semaphore::new(ASSET_CONCURRENCY));
        let mut join_set = JoinSet::new();
        let timeout = self.bytes_timeout;

        for (key, url) in tasks {
            let permit = Arc::clone(&sem);
            let http = http.clone();
            let headers = Arc::clone(&headers);
            join_set.spawn(async move {
                let Ok(_guard) = permit.acquire_owned().await else {
                    return None;
                };
                let raw = fetch_bytes_opt(&http, &url, &headers, timeout).await?;
                decode_and_binarize_cave_mask(&key, &raw)
            });
        }

        let mut archive = NpzArchive::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok(Some((key, arr))) = res {
                archive.insert(key, arr);
            }
        }
        let count = archive.len();
        archive.write_file_atomic(out_npz)?;
        Ok(count)
    }

    async fn download_terrain_tiles_async(
        &self,
        http: &reqwest::Client,
        sync_cfg: &HoyolabSyncConfig,
        maps: &BTreeMap<i32, MapMeta>,
        out_npz: &Path,
    ) -> CoreResult<BTreeMap<i32, (usize, usize)>> {
        let headers = Arc::new(build_bytes_headers(sync_cfg)?);
        let tile_base = sync_cfg.tile_base.trim().trim_end_matches('/').to_owned();
        let tile_timeout = self.bytes_timeout.min(Duration::from_secs(10));

        let mut archive = NpzArchive::new();
        let mut shapes = BTreeMap::new();

        for (&mid, mdata) in maps {
            let Some(d2) = &mdata.detail_v2 else {
                continue;
            };
            let z: i32 = if mid == 2 { -2 } else { -1 };
            let s = 2f64.powi(z);
            let z_str = if z < 0 {
                format!("N{}", -z)
            } else {
                format!("P{z}")
            };

            let px0 = d2.padding[0];
            let py0 = d2.padding[1];
            let px1 = (px0 + 256.0).max(d2.total_size[0] - px0);
            let py1 = (py0 + 256.0).max(d2.total_size[1] - py0);

            let max_tx = ((d2.total_size[0] * s - 1.0) / 256.0).floor().max(0.0) as i32;
            let max_ty = ((d2.total_size[1] * s - 1.0) / 256.0).floor().max(0.0) as i32;

            let tx_start = ((px0 * s) / 256.0).floor().max(0.0) as i32;
            let ty_start = ((py0 * s) / 256.0).floor().max(0.0) as i32;
            let tx_end = (((px1 * s).max(1.0) - 1.0) / 256.0)
                .floor()
                .min(max_tx as f64)
                .max(tx_start as f64) as i32;
            let ty_end = (((py1 * s).max(1.0) - 1.0) / 256.0)
                .floor()
                .min(max_ty as f64)
                .max(ty_start as f64) as i32;

            let cols = (tx_end - tx_start + 1) as usize;
            let rows = (ty_end - ty_start + 1) as usize;
            let canvas_w = cols * 256;
            let canvas_h = rows * 256;
            let mut canvas = vec![0u8; canvas_w * canvas_h];

            let sem = Arc::new(Semaphore::new(ASSET_CONCURRENCY));
            let mut join_set = JoinSet::new();

            for ty in ty_start..=ty_end {
                for tx in tx_start..=tx_end {
                    let permit = Arc::clone(&sem);
                    let http = http.clone();
                    let headers = Arc::clone(&headers);
                    let url =
                        format!("{tile_base}/{mid}/{}/{tx}_{ty}_{z_str}.webp", d2.map_version);
                    join_set.spawn(async move {
                        let Ok(_guard) = permit.acquire_owned().await else {
                            return None;
                        };
                        let raw = fetch_bytes_opt(&http, &url, &headers, tile_timeout).await?;
                        decode_terrain_tile(mid, tx, ty, &raw)
                    });
                }
            }

            while let Some(res) = join_set.join_next().await {
                if let Ok(Some((tx, ty, tile_bytes))) = res {
                    let ox_px = ((tx - tx_start) as usize) * 256;
                    let oy_px = ((ty - ty_start) as usize) * 256;
                    for row in 0..256usize {
                        let dst_off = (oy_px + row) * canvas_w + ox_px;
                        let src_off = row * 256;
                        canvas[dst_off..dst_off + 256]
                            .copy_from_slice(&tile_bytes[src_off..src_off + 256]);
                    }
                }
            }

            archive.insert(
                format!("map_{mid}_gray"),
                NpyArray::from_u8(vec![canvas_h, canvas_w], canvas)?,
            );
            archive.insert(
                format!("map_{mid}_meta"),
                NpyArray::from_i64(
                    vec![5],
                    vec![
                        tx_start as i64,
                        ty_start as i64,
                        z as i64,
                        d2.origin[0] as i64,
                        d2.origin[1] as i64,
                    ],
                )?,
            );
            shapes.insert(mid, (canvas_w, canvas_h));
        }

        archive.write_file_atomic(out_npz)?;
        Ok(shapes)
    }
}

impl DatasetSyncRunner for HoyolabSyncClient {
    fn check_online_map_versions(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        local_versions: &BTreeMap<i32, String>,
    ) -> CoreResult<MapVersionCheckResult> {
        if !sync_cfg.auto_check_on_startup || sync_cfg.auto_update_mode.eq_ignore_ascii_case("off")
        {
            return Ok(MapVersionCheckResult::default());
        }

        let extra_ids: Vec<i32> = local_versions.keys().copied().collect();
        let discovered = self.discover_active_maps(sync_cfg, &extra_ids)?;
        if discovered.is_empty() {
            return Err(CoreError::Sync(
                "Failed to reach HoYoLAB map API (no active maps discovered).".to_owned(),
            ));
        }

        let mut updated_maps = BTreeMap::new();
        let mut new_map_ids = Vec::new();
        for (&mid, meta) in &discovered {
            let online_ver = meta.detail_v2.map_version.trim();
            if let Some(local_ver) = local_versions.get(&mid) {
                if local_ver.trim() != online_ver {
                    updated_maps.insert(mid, (local_ver.clone(), online_ver.to_owned()));
                }
            } else {
                new_map_ids.push(mid);
            }
        }

        let teyvat_changed = updated_maps.contains_key(&2) || new_map_ids.contains(&2);
        Ok(MapVersionCheckResult {
            updated_maps,
            new_map_ids,
            teyvat_changed,
        })
    }

    fn hydrate_into(&self, staging_dir: &Path, sync_cfg: &HoyolabSyncConfig) -> CoreResult<()> {
        self.sync_to_directory_with_cfg(staging_dir, sync_cfg, None, None)?;
        Ok(())
    }
}

pub fn load_legacy_floor_shorts(legacy_path: &Path) -> BTreeMap<(i32, i32), String> {
    let mut shorts = BTreeMap::new();
    if !legacy_path.is_file() {
        return shorts;
    }
    let Ok(raw) = std::fs::read_to_string(legacy_path) else {
        return shorts;
    };
    let Ok(Value::Array(groups)) = serde_json::from_str::<Value>(&raw) else {
        return shorts;
    };
    for g in groups {
        let gid = parse_i32_tolerant(g.get("id")).unwrap_or(0);
        if gid == 0 {
            continue;
        }
        let Some(floors) = g.get("floors").and_then(Value::as_array) else {
            continue;
        };
        for fl in floors {
            let fid = parse_i32_tolerant(fl.get("id")).unwrap_or(0);
            let fs = fl
                .get("floor_short")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_ascii_uppercase();
            if fid != 0 && !fs.is_empty() {
                shorts.insert((gid, fid), fs);
            }
        }
    }
    shorts
}

pub fn update_cached_assets_atomic(
    runner: &dyn DatasetSyncRunner,
    target_cache: &Path,
    sync_cfg: &HoyolabSyncConfig,
    global_index_path: Option<&Path>,
    teyvat_changed: bool,
) -> CoreResult<PathBuf> {
    let cache_parent = target_cache
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(target_cache);
    let stage = StageGuard::create_under(cache_parent)?;

    for &name in &OPTIONAL_ASSETS {
        let existing = target_cache.join(name);
        if validate_asset_file(&existing) {
            let _ = std::fs::copy(&existing, stage.path().join(name));
        }
    }

    runner.hydrate_into(stage.path(), sync_cfg)?;

    let staged_status = verify_data_dir(stage.path(), &REQUIRED_ASSETS, false, "staged");
    if !staged_status.ready {
        return Err(CoreError::InvalidData(format!(
            "staged update incomplete (missing={:?}, corrupted={:?})",
            staged_status.missing, staged_status.corrupted
        )));
    }

    let manifest = build_manifest(
        stage.path(),
        &REQUIRED_ASSETS,
        &OPTIONAL_ASSETS,
        "sync_hoyolab_data",
    )?;
    let staged_manifest = write_manifest(stage.path(), &manifest)?;

    std::fs::create_dir_all(target_cache)?;
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen = BTreeSet::new();
    for &name in &REQUIRED_ASSETS {
        if seen.insert(name) {
            pairs.push((stage.path().join(name), target_cache.join(name)));
        }
    }
    for &name in &OPTIONAL_ASSETS {
        if seen.insert(name) {
            let candidate = stage.path().join(name);
            if candidate.is_file() {
                pairs.push((candidate, target_cache.join(name)));
            }
        }
    }
    pairs.push((staged_manifest, target_cache.join(MANIFEST_FILENAME)));

    promote_staged_assets(&pairs)?;
    drop(stage);

    let final_status = verify_data_dir(target_cache, &REQUIRED_ASSETS, true, "cache");
    if !final_status.ready {
        return Err(CoreError::InvalidData(format!(
            "promoted cache failed verification (missing={:?}, corrupted={:?}, mismatched={:?})",
            final_status.missing, final_status.corrupted, final_status.mismatched
        )));
    }

    if teyvat_changed
        && let Some(idx_path) = global_index_path
        && idx_path.is_file()
    {
        let _ = std::fs::remove_file(idx_path);
    }

    Ok(target_cache.to_path_buf())
}

pub fn check_and_apply_startup_update(
    runner: &dyn DatasetSyncRunner,
    paths: &AppPaths,
    sync_cfg: &HoyolabSyncConfig,
    local_versions: &BTreeMap<i32, String>,
) -> CoreResult<StartupUpdateOutcome> {
    if !sync_cfg.auto_check_on_startup || sync_cfg.auto_update_mode.eq_ignore_ascii_case("off") {
        return Ok(StartupUpdateOutcome::Disabled);
    }

    let res = runner.check_online_map_versions(sync_cfg, local_versions)?;
    if !res.has_updates() {
        return Ok(StartupUpdateOutcome::UpToDate(res));
    }

    if sync_cfg.auto_update_mode.eq_ignore_ascii_case("notify_only") {
        return Ok(StartupUpdateOutcome::UpdateAvailable(res));
    }

    let global_idx = paths.global_index_path();
    update_cached_assets_atomic(
        runner,
        &paths.data_dir,
        sync_cfg,
        Some(&global_idx),
        res.teyvat_changed,
    )?;
    Ok(StartupUpdateOutcome::Updated(res))
}

struct StageGuard(PathBuf);

impl StageGuard {
    fn create_under(parent: &Path) -> CoreResult<Self> {
        std::fs::create_dir_all(parent)?;
        let pid = std::process::id();
        for _ in 0..256 {
            let seq = UPDATE_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".gh_sync_stage_{pid}_{seq}"));
            if !candidate.exists() {
                std::fs::create_dir_all(&candidate)?;
                return Ok(Self(candidate));
            }
        }
        Err(CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to allocate staging directory",
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_on_worker_runtime<F, Fut, T>(f: F) -> CoreResult<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = CoreResult<T>> + 'static,
    T: Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("gh-hoyolab-sync".to_owned())
        .spawn(move || -> CoreResult<T> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| CoreError::Sync(format!("tokio runtime build failed: {e}")))?;
            rt.block_on(f())
        })
        .map_err(|e| CoreError::Sync(format!("failed spawning sync worker thread: {e}")))?;

    match handle.join() {
        Ok(res) => res,
        Err(_) => Err(CoreError::Sync("sync worker thread panicked".to_owned())),
    }
}

fn build_http_client() -> CoreResult<reqwest::Client> {
    reqwest::Client::builder()
        .build()
        .map_err(|e| CoreError::Sync(format!("http client build failed: {e}")))
}

fn build_api_headers(sync_cfg: &HoyolabSyncConfig) -> CoreResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    let ua = HeaderValue::from_str(sync_cfg.user_agent.trim())
        .map_err(|e| CoreError::InvalidArgument(format!("invalid user_agent header: {e}")))?;
    let ver = HeaderValue::from_str(sync_cfg.rpc_map_version.trim())
        .map_err(|e| CoreError::InvalidArgument(format!("invalid rpc_map_version: {e}")))?;
    headers.insert(USER_AGENT, ua);
    headers.insert(HEADER_RPC_MAP_VERSION, ver);
    Ok(headers)
}

fn build_bytes_headers(sync_cfg: &HoyolabSyncConfig) -> CoreResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    let ua = HeaderValue::from_str(sync_cfg.user_agent.trim())
        .map_err(|e| CoreError::InvalidArgument(format!("invalid user_agent header: {e}")))?;
    headers.insert(USER_AGENT, ua);
    Ok(headers)
}

async fn fetch_json_opt(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    timeout: Duration,
) -> Option<Value> {
    let resp = match client
        .get(url)
        .headers(headers.clone())
        .timeout(timeout)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            eprintln!("[gh-net::hoyolab_sync] request failed ({url}): {err}");
            return None;
        }
    };
    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return None;
    }
    if !status.is_success() {
        eprintln!("[gh-net::hoyolab_sync] HTTP {status} fetching {url}");
        return None;
    }
    match resp.json::<Value>().await {
        Ok(val) => Some(val),
        Err(err) => {
            eprintln!("[gh-net::hoyolab_sync] invalid JSON from {url}: {err}");
            None
        }
    }
}

async fn fetch_bytes_opt(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

fn decode_and_binarize_cave_mask(key: &str, raw: &[u8]) -> Option<(String, NpyArray)> {
    let img = image::load_from_memory(raw).ok()?;
    let (mw, mh) = (img.width(), img.height());
    if mw == 0 || mh == 0 {
        return None;
    }

    let bin: Vec<u8> = if img.color().has_alpha() {
        img.to_rgba8()
            .pixels()
            .map(|p| u8::from(p[3] > 30))
            .collect()
    } else {
        img.to_luma8()
            .pixels()
            .map(|p| u8::from(p[0] > 10))
            .collect()
    };

    let max_dim = mw.max(mh);
    let (final_w, final_h, final_buf) = if max_dim > 256 {
        let scale = 256.0 / (max_dim as f64);
        let new_w = ((mw as f64) * scale).round().max(8.0) as u32;
        let new_h = ((mh as f64) * scale).round().max(8.0) as u32;
        let src = ImageBuffer::<Luma<u8>, Vec<u8>>::from_raw(mw, mh, bin)?;
        let resized = image::imageops::resize(&src, new_w, new_h, FilterType::Nearest);
        (new_w as usize, new_h as usize, resized.into_raw())
    } else {
        (mw as usize, mh as usize, bin)
    };

    let arr = NpyArray::from_u8(vec![final_h, final_w], final_buf).ok()?;
    Some((key.to_owned(), arr))
}

fn decode_terrain_tile(mid: i32, tx: i32, ty: i32, raw: &[u8]) -> Option<(i32, i32, Vec<u8>)> {
    let img = match image::load_from_memory(raw) {
        Ok(i) => i,
        Err(err) => {
            eprintln!("[gh-net::hoyolab_sync] corrupt tile {mid}/{tx}_{ty}: {err}");
            return None;
        }
    };
    let gray = img.to_luma8();
    let resized = if gray.width() != 256 || gray.height() != 256 {
        image::imageops::resize(&gray, 256, 256, FilterType::Triangle)
    } else {
        gray
    };
    Some((tx, ty, resized.into_raw()))
}

fn is_retcode_ok(data: &Value) -> bool {
    match data.get("retcode") {
        Some(Value::Number(n)) => n.as_i64() == Some(0),
        Some(Value::String(s)) => s.trim() == "0",
        _ => false,
    }
}

fn parse_i64_tolerant(val: Option<&Value>) -> Option<i64> {
    match val? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok())).or_else(|| {
            n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)
        }),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<i64>().ok().or_else(|| t.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f as i64))
        }
        _ => None,
    }
}

fn parse_i32_tolerant(val: Option<&Value>) -> Option<i32> {
    parse_i64_tolerant(val).and_then(|v| i32::try_from(v).ok())
}

fn parse_f64_tolerant(val: Option<&Value>) -> Option<f64> {
    let f = match val? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if f.is_finite() { Some(f) } else { None }
}

fn parse_f64_pair_tolerant(val: Option<&Value>) -> Option<[f64; 2]> {
    let arr = val?.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    Some([
        parse_f64_tolerant(Some(&arr[0]))?,
        parse_f64_tolerant(Some(&arr[1]))?,
    ])
}

fn parse_detail_v2(raw: &Value) -> Option<DetailV2Meta> {
    let owned: Value;
    let obj_val = match raw {
        Value::String(s) => {
            owned = serde_json::from_str::<Value>(s).ok()?;
            &owned
        }
        Value::Object(_) => raw,
        _ => return None,
    };
    let obj = obj_val.as_object()?;
    let map_version = obj
        .get("map_version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let total_size = parse_f64_pair_tolerant(obj.get("total_size")).unwrap_or([0.0, 0.0]);
    let padding = parse_f64_pair_tolerant(obj.get("padding")).unwrap_or([0.0, 0.0]);
    let origin = parse_f64_pair_tolerant(obj.get("origin")).unwrap_or([0.0, 0.0]);
    let original_map_size = parse_f64_pair_tolerant(obj.get("original_map_size"));
    let min_zoom = parse_i32_tolerant(obj.get("min_zoom")).unwrap_or(0);
    let max_zoom = parse_i32_tolerant(obj.get("max_zoom")).unwrap_or(0);

    Some(DetailV2Meta {
        map_version,
        total_size,
        padding,
        origin,
        original_map_size,
        min_zoom,
        max_zoom,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::{Path as AxumPath, Query, State};
    use axum::http::HeaderMap as AxumHeaders;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use gh_core::assets::{EnsureDataOptions, ensure_data_ready, read_manifest};
    use gh_core::paths::{PathEnv, RuntimeMode};
    use image::{ImageFormat, Rgba, RgbaImage};
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_net_hoyolab_sync_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn encode_rgba_png(w: u32, h: u32, alpha_fn: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Rgba([200, 180, 160, alpha_fn(x, y)]));
            }
        }
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).expect("encode png");
        buf.into_inner()
    }

    fn encode_luma_png(w: u32, h: u32, luma_fn: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut img = ImageBuffer::<Luma<u8>, Vec<u8>>::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Luma([luma_fn(x, y)]));
            }
        }
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).expect("encode png");
        buf.into_inner()
    }

    fn encode_luma_webp(w: u32, h: u32, val: u8) -> Vec<u8> {
        let mut img = ImageBuffer::<Luma<u8>, Vec<u8>>::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Luma([val.wrapping_add((x % 17) as u8)]));
            }
        }
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::WebP).expect("encode webp");
        buf.into_inner()
    }

    #[derive(Clone)]
    struct MockState {
        map2_version: String,
        map7_version: String,
        include_map40: bool,
        corrupt_tile: bool,
        info_calls: Arc<AtomicUsize>,
        last_rpc_header: Arc<std::sync::Mutex<Option<String>>>,
        rgba_mask_png: Vec<u8>,
        luma_mask_png: Vec<u8>,
        tile_webp: Vec<u8>,
    }

    async fn mock_map_info(
        State(st): State<MockState>,
        headers: AxumHeaders,
        Query(q): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        st.info_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(h) = headers.get("x-rpc-map_version").and_then(|v| v.to_str().ok()) {
            *st.last_rpc_header.lock().unwrap() = Some(h.to_owned());
        }
        let mid: i32 = q.get("map_id").and_then(|s| s.parse().ok()).unwrap_or(0);
        match mid {
            2 => {
                // Stringified detail_v2
                let d2 = json!({
                    "map_version": st.map2_version,
                    "total_size": [2048, 2048],
                    "padding": [0, 0],
                    "origin": [1024, 1024],
                    "min_zoom": -4,
                    "max_zoom": 0
                })
                .to_string();
                axum::Json(json!({
                    "retcode": 0,
                    "data": { "info": { "id": 2, "name": "Teyvat", "detail_v2": d2 } }
                }))
                .into_response()
            }
            7 => {
                // Inline object detail_v2
                axum::Json(json!({
                    "retcode": 0,
                    "data": {
                        "info": {
                            "id": 7,
                            "name": "Enkanomiya",
                            "detail_v2": {
                                "map_version": st.map7_version,
                                "total_size": [1024, 1024],
                                "padding": [0, 0],
                                "origin": [512, 512],
                                "min_zoom": -3,
                                "max_zoom": 0
                            }
                        }
                    }
                }))
                .into_response()
            }
            40 if st.include_map40 => axum::Json(json!({
                "retcode": 0,
                "data": {
                    "info": {
                        "id": 40,
                        "name": "Nod-Krai",
                        "detail_v2": {
                            "map_version": "v40",
                            "total_size": [512, 512],
                            "padding": [0, 0],
                            "origin": [256, 256]
                        }
                    }
                }
            }))
            .into_response(),
            _ => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    async fn mock_areas(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
        let mid: i32 = q.get("map_id").and_then(|s| s.parse().ok()).unwrap_or(0);
        axum::Json(json!({
            "retcode": 0,
            "data": { "list": [{ "id": mid * 10, "name": format!("Area {mid}") }] }
        }))
    }

    async fn mock_label_tree() -> impl IntoResponse {
        axum::Json(json!({
            "retcode": 0,
            "data": {
                "tree": [
                    {
                        "id": 1,
                        "name": "Landmarks",
                        "children": [
                            { "id": 2, "name": "Teleport Waypoint" },
                            { "id": 3, "name": "Statue of The Seven" },
                            { "id": 18, "name": "Chest" }
                        ]
                    }
                ]
            }
        }))
    }

    async fn mock_point_list(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
        let mid: i32 = q.get("map_id").and_then(|s| s.parse().ok()).unwrap_or(0);
        axum::Json(json!({
            "retcode": 0,
            "data": {
                "point_list": [
                    {
                        "id": "900001",
                        "label_id": 2,
                        "x_pos": "120.5",
                        "y_pos": -45.25,
                        "area_id": mid * 10,
                        "z_level": 1,
                        "icon_sign": 0,
                        "point_group": { "group_id": 100, "floor_id": 501 }
                    },
                    {
                        "id": 900002,
                        "label_id": 999,
                        "x_pos": 0.0,
                        "y_pos": 0.0
                    }
                ]
            }
        }))
    }

    async fn mock_point_group(
        State(_st): State<MockState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        let mid: i32 = q.get("map_id").and_then(|s| s.parse().ok()).unwrap_or(0);
        if mid == 2 {
            axum::Json(json!({
                "retcode": 0,
                "data": {
                    "list": [{
                        "id": 100,
                        "name": "Ruins",
                        "g_floor_name": "",
                        "default_floor_index": 0,
                        "floors": [
                            {
                                "id": 501,
                                "floor_name": "Lower",
                                "floor_name_short": " b1 ",
                                "overlay": {
                                    "url": format!("{}/mask/rgba.png", st_base_placeholder()),
                                    "l_x": 10, "l_y": 20, "r_x": 50, "r_y": 80
                                }
                            },
                            {
                                "id": 502,
                                "floor_name": "Deep",
                                "floor_name_short": "",
                                "overlay": {
                                    "url": format!("{}/mask/luma.png", st_base_placeholder()),
                                    "l_x": 0, "l_y": 0, "r_x": 40, "r_y": 40
                                }
                            }
                        ]
                    }]
                }
            }))
        } else {
            axum::Json(json!({ "retcode": 0, "data": { "list": [] } }))
        }
    }

    fn st_base_placeholder() -> &'static str {
        "MOCK_BASE"
    }

    async fn spawn_mock_hoyolab(
        state: MockState,
    ) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let base_url = format!("http://{addr}");

        let state_pg = state.clone();
        let app = Router::new()
            .route("/api/v1/map/info", get(mock_map_info))
            .route("/api/v1/map/get_area_pageLabel", get(mock_areas))
            .route("/api/v1/map/label/tree", get(mock_label_tree))
            .route("/api/v2/map/point/list", get(mock_point_list))
            .route(
                "/api/v2/map/point_group",
                get(move |q: Query<HashMap<String, String>>| {
                    let st = state_pg.clone();
                    let base = base_url.clone();
                    async move {
                        let resp = mock_point_group(State(st), q).await.into_response();
                        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                            .await
                            .unwrap();
                        let s = String::from_utf8(body.to_vec())
                            .unwrap()
                            .replace("MOCK_BASE", &base);
                        ([(axum::http::header::CONTENT_TYPE, "application/json")], s)
                    }
                }),
            )
            .route(
                "/mask/{kind}",
                get(|State(st): State<MockState>, AxumPath(kind): AxumPath<String>| async move {
                    let bytes = if kind == "rgba.png" {
                        st.rgba_mask_png
                    } else {
                        st.luma_mask_png
                    };
                    ([(axum::http::header::CONTENT_TYPE, "image/png")], bytes)
                }),
            )
            .route(
                "/tiles/{mid}/{ver}/{file}",
                get(
                    |State(st): State<MockState>,
                     AxumPath((mid, _ver, file)): AxumPath<(i32, String, String)>| async move {
                        if st.corrupt_tile && mid == 2 && file.starts_with("0_0_") {
                            return (
                                [(axum::http::header::CONTENT_TYPE, "image/webp")],
                                b"not_a_webp".to_vec(),
                            )
                                .into_response();
                        }
                        (
                            [(axum::http::header::CONTENT_TYPE, "image/webp")],
                            st.tile_webp,
                        )
                            .into_response()
                    },
                ),
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
        (addr, tx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_sync_and_ensure_data_ready_end_to_end() {
        let state = MockState {
            map2_version: "v2_alpha".to_owned(),
            map7_version: "v7_alpha".to_owned(),
            include_map40: false,
            corrupt_tile: true,
            info_calls: Arc::new(AtomicUsize::new(0)),
            last_rpc_header: Arc::new(std::sync::Mutex::new(None)),
            // >256px RGBA mask: 512x256 -> downscaled to 256x128
            rgba_mask_png: encode_rgba_png(512, 256, |x, _| if x >= 256 { 200 } else { 10 }),
            // Gray mask: 32x16
            luma_mask_png: encode_luma_png(32, 16, |_, y| if y >= 8 { 100 } else { 5 }),
            tile_webp: encode_luma_webp(256, 256, 80),
        };
        let (addr, shutdown_tx) = spawn_mock_hoyolab(state.clone()).await;

        let base = make_temp_dir("e2e");
        let cache_dir = base.join("cache_data");
        let empty_bundle = base.join("empty_bundle");
        let legacy_ug = base.join("underground_db.json");
        std::fs::create_dir_all(&empty_bundle).unwrap();
        std::fs::write(
            &legacy_ug,
            r#"[{"id": 100, "floors": [{"id": 502, "floor_short": "b2"}]}]"#,
        )
        .unwrap();

        let mut cfg = HoyolabSyncConfig {
            base_api: format!("http://{addr}/api"),
            tile_base: format!("http://{addr}/tiles"),
            default_map_ids: vec![2, 7],
            probe_id_range: [1, 10],
            ..HoyolabSyncConfig::default()
        };
        cfg.normalize();

        let runner = HoyolabSyncClient::with_timeouts(
            Duration::from_secs(3),
            Duration::from_secs(3),
            Duration::from_secs(3),
        );

        // First test legacy_shorts loading + sync_to_directory_with_cfg directly
        let direct_dir = base.join("direct");
        runner
            .sync_to_directory_with_cfg(&direct_dir, &cfg, Some(&legacy_ug), None)
            .expect("direct sync");
        let db = MultimapDb::from_json_str(
            &std::fs::read_to_string(direct_dir.join("multimap_v4.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(db.maps.len(), 2);
        assert_eq!(db.map(2).unwrap().point_groups[0].g_floor_name, "G");
        assert_eq!(
            db.map(2).unwrap().point_groups[0].floors[0].floor_name_short,
            "B1"
        );
        assert_eq!(
            db.map(2).unwrap().point_groups[0].floors[1].floor_name_short,
            "B2"
        );
        // Only label 2 waypoint kept, label 999 filtered out
        assert_eq!(db.waypoints_for_map(2).len(), 1);
        assert_eq!(db.waypoints_for_map(2)[0].id, 900001);

        // Verify cave_masks_v4.npz dtypes, downscaling, and threshold binarization
        let masks_npz = NpzArchive::read_file(&direct_dir.join("cave_masks_v4.npz")).unwrap();
        let m501 = masks_npz.get("2_100_501").unwrap();
        assert_eq!(m501.shape(), &[128, 256]);
        let u501 = m501.as_u8().unwrap();
        assert_eq!(u501[0], 0);
        assert_eq!(u501[255], 1);
        let m502 = masks_npz.get("2_100_502").unwrap();
        assert_eq!(m502.shape(), &[16, 32]);
        assert_eq!(m502.as_u8().unwrap()[0], 0);
        assert_eq!(m502.as_u8().unwrap()[15 * 32], 1);

        // Verify terrain_n2_v4.npz shapes and metadata ([tx_start, ty_start, z, ox, oy])
        let terrain_npz = NpzArchive::read_file(&direct_dir.join("terrain_n2_v4.npz")).unwrap();
        let m2_meta = terrain_npz.get("map_2_meta").unwrap().as_i64().unwrap();
        assert_eq!(m2_meta, &[0, 0, -2, 1024, 1024]);
        // Map 2 total_size=2048, z=-2 (s=0.25) -> 512px -> 2x2 tiles -> 512x512
        let m2_gray = terrain_npz.get("map_2_gray").unwrap();
        assert_eq!(m2_gray.shape(), &[512, 512]);
        // Tile (0,0) was corrupt so skipped (remains 0); tile (1,0) succeeded (>0)
        assert_eq!(m2_gray.as_u8().unwrap()[0], 0);
        assert!(m2_gray.as_u8().unwrap()[256] >= 80);

        let m7_meta = terrain_npz.get("map_7_meta").unwrap().as_i64().unwrap();
        assert_eq!(m7_meta, &[0, 0, -1, 512, 512]);
        assert_eq!(terrain_npz.get("map_7_gray").unwrap().shape(), &[512, 512]);

        // Now test gh_core::assets::ensure_data_ready end-to-end
        let calls_before = state.info_calls.load(Ordering::SeqCst);
        let opts = EnsureDataOptions {
            cache_dir: Some(&cache_dir),
            fallback_bundle_dir: Some(&empty_bundle),
            required_assets: &REQUIRED_ASSETS,
            auto_sync: true,
            sync_cfg: &cfg,
        };
        let out = ensure_data_ready(&opts, Some(&runner)).expect("ensure_data_ready");
        assert_eq!(out, cache_dir);
        let calls_after_first = state.info_calls.load(Ordering::SeqCst);
        assert!(calls_after_first > calls_before);

        let manifest = read_manifest(&cache_dir).expect("manifest");
        assert_eq!(manifest.source, "sync_hoyolab_data");
        assert_eq!(manifest.files.len(), 3);

        // Second call hits cache with 0 network requests
        let out2 = ensure_data_ready(&opts, Some(&runner)).expect("cache hit");
        assert_eq!(out2, cache_dir);
        assert_eq!(state.info_calls.load(Ordering::SeqCst), calls_after_first);

        // Tamper detection
        std::fs::write(cache_dir.join("multimap_v4.json"), r#"{"version":"bad"}"#).unwrap();
        let status = verify_data_dir(&cache_dir, &REQUIRED_ASSETS, true, "cache");
        assert!(!status.ready);
        assert_eq!(status.mismatched, vec!["multimap_v4.json".to_owned()]);

        assert_eq!(
            state.last_rpc_header.lock().unwrap().as_deref(),
            Some("4.5")
        );

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_version_check_modes_and_teyvat_index_invalidation() {
        let state = MockState {
            map2_version: "v2_new".to_owned(),
            map7_version: "v7_old".to_owned(),
            include_map40: true,
            corrupt_tile: false,
            info_calls: Arc::new(AtomicUsize::new(0)),
            last_rpc_header: Arc::new(std::sync::Mutex::new(None)),
            rgba_mask_png: encode_rgba_png(16, 16, |_, _| 255),
            luma_mask_png: encode_luma_png(16, 16, |_, _| 255),
            tile_webp: encode_luma_webp(256, 256, 100),
        };
        let (addr, shutdown_tx) = spawn_mock_hoyolab(state.clone()).await;

        let base = make_temp_dir("startup_modes");
        let local_app = base.join("local");
        let data_dir = local_app.join("GenshinHelper").join("data");
        let cache_dir = local_app.join("GenshinHelper").join("cache");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        let paths = AppPaths {
            mode: RuntimeMode::Installed,
            config_path: base.join("config.json"),
            diagnostics_dir: base.join("diag"),
            data_dir: data_dir.clone(),
            tiles_dir: base.join("tiles"),
            ug_overlays_dir: base.join("ug"),
            cache_dir: cache_dir.clone(),
            dev_data_dir: None,
        };
        let _ = PathEnv::default();

        let runner = HoyolabSyncClient::default();
        let mut local_versions = BTreeMap::new();
        local_versions.insert(2, "v2_old".to_owned());
        local_versions.insert(7, "v7_old".to_owned());

        // 1. auto_check_on_startup = false -> 0 requests
        let cfg_off_check = HoyolabSyncConfig {
            base_api: format!("http://{addr}/api"),
            tile_base: format!("http://{addr}/tiles"),
            default_map_ids: vec![2, 7],
            probe_id_range: [40, 40],
            auto_check_on_startup: false,
            auto_update_mode: "auto_download".to_owned(),
            ..HoyolabSyncConfig::default()
        };
        assert_eq!(
            check_and_apply_startup_update(&runner, &paths, &cfg_off_check, &local_versions)
                .unwrap(),
            StartupUpdateOutcome::Disabled
        );
        assert_eq!(state.info_calls.load(Ordering::SeqCst), 0);

        // 2. auto_update_mode = "off" -> 0 requests
        let cfg_off_mode = HoyolabSyncConfig {
            auto_check_on_startup: true,
            auto_update_mode: "off".to_owned(),
            ..cfg_off_check.clone()
        };
        assert_eq!(
            check_and_apply_startup_update(&runner, &paths, &cfg_off_mode, &local_versions)
                .unwrap(),
            StartupUpdateOutcome::Disabled
        );
        assert_eq!(state.info_calls.load(Ordering::SeqCst), 0);

        // 3. notify_only -> detects Map 2 changed + Map 40 new, without downloading
        let cfg_notify = HoyolabSyncConfig {
            auto_check_on_startup: true,
            auto_update_mode: "notify_only".to_owned(),
            ..cfg_off_check.clone()
        };
        let notify_out =
            check_and_apply_startup_update(&runner, &paths, &cfg_notify, &local_versions).unwrap();
        match notify_out {
            StartupUpdateOutcome::UpdateAvailable(res) => {
                assert!(res.teyvat_changed);
                assert_eq!(
                    res.updated_maps.get(&2),
                    Some(&("v2_old".to_owned(), "v2_new".to_owned()))
                );
                assert_eq!(res.new_map_ids, vec![40]);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert!(!data_dir.join("multimap_v4.json").exists());

        // 4. Seed global index; first test when ONLY Map 7 changes (global index preserved)
        let global_idx = paths.global_index_path();
        std::fs::write(&global_idx, b"keep_me").unwrap();
        let mut only_map7_stale = BTreeMap::new();
        only_map7_stale.insert(2, "v2_new".to_owned());
        only_map7_stale.insert(7, "v7_stale".to_owned());
        only_map7_stale.insert(40, "v40".to_owned());

        let cfg_auto = HoyolabSyncConfig {
            auto_check_on_startup: true,
            auto_update_mode: "auto_download".to_owned(),
            ..cfg_off_check.clone()
        };
        let res7 =
            check_and_apply_startup_update(&runner, &paths, &cfg_auto, &only_map7_stale).unwrap();
        assert!(matches!(
            res7,
            StartupUpdateOutcome::Updated(ref r) if !r.teyvat_changed
        ));
        assert!(global_idx.is_file());
        assert!(data_dir.join("multimap_v4.json").is_file());

        // 5. Now test Map 2 changed -> invalidates global relocator index
        let res2 =
            check_and_apply_startup_update(&runner, &paths, &cfg_auto, &local_versions).unwrap();
        assert!(matches!(
            res2,
            StartupUpdateOutcome::Updated(ref r) if r.teyvat_changed
        ));
        assert!(!global_idx.exists());

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn update_cached_assets_atomic_preserves_existing_cache_on_hydrate_failure() {
        let base = make_temp_dir("rollback");
        let cache_dir = base.join("data");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("multimap_v4.json"), r#"{"version":"orig"}"#).unwrap();

        let failing_runner = gh_core::assets::ClosureSyncRunner(|stage, _cfg| {
            std::fs::write(stage.join("multimap_v4.json"), r#"{"version":"partial"}"#)?;
            Err(CoreError::Sync("simulated network error".to_owned()))
        });
        let err = update_cached_assets_atomic(
            &failing_runner,
            &cache_dir,
            &HoyolabSyncConfig::default(),
            None,
            true,
        )
        .expect_err("must fail");
        assert!(matches!(err, CoreError::Sync(_)));
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("multimap_v4.json")).unwrap(),
            r#"{"version":"orig"}"#
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
