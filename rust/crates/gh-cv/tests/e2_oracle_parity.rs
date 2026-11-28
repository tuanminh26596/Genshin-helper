//! Track E2 `#[ignore]` Oracle Parity Tests on real `scratch/` fixtures.
//! Run with:
//! `cargo nextest run --manifest-path rust/Cargo.toml -p gh-cv --profile cv-heavy --cargo-profile cv-bench --run-ignored all`

use gh_core::frame::{FrameBuf, PixelFormat};
use gh_core::multimap::load_multimap_db;
use gh_core::npz::{NpyData, NpzArchive};
use gh_core::traits::UndergroundOverlaySource;
use gh_core::ui_layout::compute_ui_layout;
use gh_cv::global_relocator::{GLOBAL_INDEX_FILENAME, INDEX_VERSION, TeyvatGlobalRelocator};
use gh_cv::heading360::{shortest_angle_delta, wrap_angle_360};
use gh_cv::image_io::{ImageReadMode, DecodedImage, read_image, read_image_bgr, write_image_gray};
use gh_cv::locator::TerrainRaster;
use gh_cv::map_state::MapScreenStateMachine;
use gh_cv::minimap_tracker::{
    HybridMinimapTracker, LocalizationScheduler, LocalizationWorker, MinimapGate, MinimapMode,
    MinimapTrackerConfig, TrackerState, ZoomRegime, visible_snapshot,
};
use gh_cv::ops::color::bgr_to_gray;
use gh_cv::ops::morph::{
    draw_filled_circle_u8, draw_filled_convex_poly_u8, draw_filled_ellipse_sector_u8,
};
use gh_cv::ops::ncc::{match_template_f32, min_max_loc_f32};
use gh_cv::ops::resize::{resize_u8, resize_u8_hwc};
use gh_cv::ops::warp::{get_rotation_matrix_2d, warp_affine_u8};
use gh_cv::ops::{BorderMode, Interpolation, Point2i, TemplateMatchMode};
use gh_cv::tile_cache::{
    LRU_TILE_CAPACITY, LruTerrainTileCache, OverlayBounds, TileCacheConfig,
    compose_underground_local_to_surface, extract_underground_patch_gray,
};
use image::{ImageBuffer, Rgba};
use ndarray::{Array2, Array3, ArrayView2, s};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

fn terrain_npz_path(root: &Path) -> PathBuf {
    root.join("data").join("terrain_n2_v4.npz")
}

fn multimap_json_path(root: &Path) -> PathBuf {
    root.join("data").join("multimap_v4.json")
}

fn load_map2_raster(root: &Path) -> Option<Arc<TerrainRaster>> {
    let p = terrain_npz_path(root);
    if !p.is_file() {
        return None;
    }
    let gray_arr = NpzArchive::read_single_from_file(&p, "map_2_gray").ok()?;
    let meta_arr = NpzArchive::read_single_from_file(&p, "map_2_meta").ok()?;
    if gray_arr.shape.len() != 2 {
        return None;
    }
    let NpyData::U8(raw) = gray_arr.data else {
        return None;
    };
    let gray = Array2::from_shape_vec((gray_arr.shape[0], gray_arr.shape[1]), raw).ok()?;
    let vals: Vec<f64> = match meta_arr.data {
        NpyData::I64(v) => v.into_iter().map(|x| x as f64).collect(),
        NpyData::I32(v) => v.into_iter().map(|x| x as f64).collect(),
        NpyData::F32(v) => v.into_iter().map(|x| x as f64).collect(),
        NpyData::U8(v) => v.into_iter().map(|x| x as f64).collect(),
    };
    if vals.len() < 5 {
        return None;
    }
    Some(Arc::new(TerrainRaster {
        gray,
        meta: [vals[0], vals[1], vals[2], vals[3], vals[4]],
    }))
}

fn draw_cyan_player_arrow(img_bgr: &mut Array3<u8>, heading_cw_deg: f64) {
    let (h, w, c) = img_bgr.dim();
    let cx = w as f64 * 0.5;
    let cy = h as f64 * 0.5;
    let s = (h.min(w) as f64) / 218.0;
    let pts_local = [
        [0.0, -14.0 * s],
        [10.0 * s, 9.0 * s],
        [0.0, 4.0 * s],
        [-10.0 * s, 9.0 * s],
    ];
    let rad = heading_cw_deg.to_radians();
    let (sn, cs) = rad.sin_cos();
    let rot: Vec<Point2i> = pts_local
        .iter()
        .map(|[lx, ly]| Point2i {
            x: (cs * lx - sn * ly + cx).round_ties_even() as i32,
            y: (sn * lx + cs * ly + cy).round_ties_even() as i32,
        })
        .collect();

    let mut arrow_mask = Array2::<u8>::zeros((h, w));
    draw_filled_convex_poly_u8(&mut arrow_mask, &[rot[0], rot[1], rot[2]], 255);
    draw_filled_convex_poly_u8(&mut arrow_mask, &[rot[0], rot[2], rot[3]], 255);
    for y in 0..h {
        for x in 0..w {
            if arrow_mask[[y, x]] > 0 {
                img_bgr[[y, x, 0]] = 252;
                img_bgr[[y, x, 1]] = 235;
                img_bgr[[y, x, 2]] = 10;
                if c == 4 {
                    img_bgr[[y, x, 3]] = 255;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_synthetic_minimap(
    tile_cache: &LruTerrainTileCache,
    map_id: i32,
    world_x: f64,
    world_y: f64,
    span_world: f64,
    size: usize,
    camera_heading_cw: f64,
    body_heading_cw: f64,
    follow_view: bool,
    with_bgra_alpha: bool,
    custom_terrain_gray: Option<ArrayView2<'_, u8>>,
) -> Array3<u8> {
    let base_gray = if let Some(cg) = custom_terrain_gray {
        resize_u8(cg, size, size, Interpolation::Linear).unwrap()
    } else {
        let ppu = size as f64 / span_world;
        let half_ext = 0.5 * span_world * if follow_view { 1.48 } else { 1.02 };
        let (patch_opt, _) =
            tile_cache.extract_local_patch(map_id, world_x, world_y, half_ext, ppu);
        let patch = patch_opt.expect("extract_local_patch failed in render_synthetic_minimap");
        let (ph, pw) = patch.dim();
        if follow_view {
            let m_rot = get_rotation_matrix_2d(
                ((pw - 1) as f64 * 0.5, (ph - 1) as f64 * 0.5),
                camera_heading_cw,
                1.0,
            );
            let rot = warp_affine_u8(
                patch.view(),
                &m_rot,
                pw,
                ph,
                Interpolation::Linear,
                false,
                BorderMode::Replicate,
            )
            .unwrap();
            let ox = (pw - size) / 2;
            let oy = (ph - size) / 2;
            rot.slice(s![oy..oy + size, ox..ox + size]).to_owned()
        } else {
            let ox = pw.saturating_sub(size) / 2;
            let oy = ph.saturating_sub(size) / 2;
            let crop = patch.slice(s![oy..ph.min(oy + size), ox..pw.min(ox + size)]);
            if crop.dim() == (size, size) {
                crop.to_owned()
            } else {
                resize_u8(crop, size, size, Interpolation::Linear).unwrap()
            }
        }
    };

    render_minimap_from_gray(
        base_gray.view(),
        size,
        camera_heading_cw,
        body_heading_cw,
        follow_view,
        with_bgra_alpha,
    )
}

fn render_minimap_from_gray(
    base_in: ArrayView2<'_, u8>,
    size: usize,
    camera_heading_cw: f64,
    body_heading_cw: f64,
    follow_view: bool,
    with_bgra_alpha: bool,
) -> Array3<u8> {
    let base_gray = if base_in.dim() == (size, size) {
        base_in.to_owned()
    } else {
        let (ph, pw) = base_in.dim();
        if ph >= size && pw >= size {
            let ox = (pw - size) / 2;
            let oy = (ph - size) / 2;
            base_in.slice(s![oy..oy + size, ox..ox + size]).to_owned()
        } else {
            resize_u8(base_in, size, size, Interpolation::Linear).unwrap()
        }
    };

    let ch = if with_bgra_alpha { 4 } else { 3 };
    let mut img = Array3::<u8>::zeros((size, size, ch));
    for y in 0..size {
        for x in 0..size {
            let g = base_gray[[y, x]] as f32;
            img[[y, x, 0]] = (g * 0.88).clamp(0.0, 255.0) as u8;
            img[[y, x, 1]] = (g * 0.96).clamp(0.0, 255.0) as u8;
            img[[y, x, 2]] = (g * 0.92).clamp(0.0, 255.0) as u8;
            if ch == 4 {
                img[[y, x, 3]] = 255;
            }
        }
    }

    let c = (size / 2) as f64;
    let outer_r = (0.46 * size as f64).round_ties_even();
    let cone_ang = if follow_view {
        0.0
    } else {
        wrap_angle_360(camera_heading_cw)
    };
    let cv_center_ang = cone_ang - 90.0;
    let cone_r = (0.43 * size as f64).round_ties_even();
    let mut cone_mask = Array2::<u8>::zeros((size, size));
    draw_filled_ellipse_sector_u8(
        &mut cone_mask,
        c,
        c,
        cone_r,
        cone_r,
        cv_center_ang - 45.0,
        cv_center_ang + 45.0,
        255,
    );

    let mut circle_mask = Array2::<u8>::zeros((size, size));
    draw_filled_circle_u8(&mut circle_mask, c as i32, c as i32, outer_r as i32, 255);

    for y in 0..size {
        for x in 0..size {
            if cone_mask[[y, x]] > 0 {
                for k in 0..3 {
                    let v = img[[y, x, k]] as f32 * 0.75 + 255.0 * 0.25;
                    img[[y, x, k]] = v.clamp(0.0, 255.0) as u8;
                }
                if ch == 4 {
                    img[[y, x, 3]] = 191;
                }
            }
            if circle_mask[[y, x]] == 0 {
                img[[y, x, 0]] = 18;
                img[[y, x, 1]] = 20;
                img[[y, x, 2]] = 24;
            }
            let rr = (x as f64 - c).hypot(y as f64 - c);
            if (rr - (outer_r + 2.0)).abs() <= 1.0 {
                img[[y, x, 0]] = 45;
                img[[y, x, 1]] = 48;
                img[[y, x, 2]] = 52;
            }
            if (rr - outer_r).abs() <= 1.0 {
                img[[y, x, 0]] = 215;
                img[[y, x, 1]] = 224;
                img[[y, x, 2]] = 232;
            }
        }
    }

    let arrow_ang = if follow_view {
        wrap_angle_360(body_heading_cw - camera_heading_cw)
    } else {
        wrap_angle_360(body_heading_cw)
    };
    draw_cyan_player_arrow(&mut img, arrow_ang);
    img
}

fn make_synthetic_badge(w: usize, h: usize) -> (Array3<u8>, Array2<u8>) {
    let mut bgr = Array3::<u8>::from_elem((h, w, 3), 24);
    let mut state = 20261122u64;
    let mut next_u8 = || -> u8 {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        ((state >> 24) % 12) as u8
    };
    for y in 0..h {
        for x in 0..w {
            for c in 0..3 {
                bgr[[y, x, c]] = bgr[[y, x, c]].saturating_add(next_u8());
            }
        }
    }
    let mut mask = Array2::<u8>::zeros((h, w));
    for y in 0..h {
        let dy = (y as f64 - 38.0) / 35.0;
        for x in 0..w {
            let dx = (x as f64 - 33.5) / 30.0;
            if dx * dx + dy * dy <= 1.0 {
                mask[[y, x]] = 255;
            }
            let rr = (x as f64 - 34.0).hypot(y as f64 - 38.0);
            if rr <= 22.0 {
                bgr[[y, x, 0]] = 70;
                bgr[[y, x, 1]] = 95;
                bgr[[y, x, 2]] = 165;
            }
            if (rr - 28.0).abs() <= 1.2 {
                bgr[[y, x, 0]] = 215;
                bgr[[y, x, 1]] = 225;
                bgr[[y, x, 2]] = 240;
            }
            if (x as isize - 34).abs() + (y as isize - 36).abs() <= 14 {
                bgr[[y, x, 0]] = 242;
                bgr[[y, x, 1]] = 232;
                bgr[[y, x, 2]] = 195;
            }
            if (x as isize - y as isize).abs() <= 2 && (20..=48).contains(&x) {
                bgr[[y, x, 0]] = 40;
                bgr[[y, x, 1]] = 48;
                bgr[[y, x, 2]] = 88;
            }
        }
    }
    (bgr, mask)
}

fn compose_hud_frame(
    minimap: &Array3<u8>,
    badge: Option<&Array3<u8>>,
    frame_w: u32,
    frame_h: u32,
    dx_px: i32,
    dy_px: i32,
) -> Array3<u8> {
    let lay = compute_ui_layout(frame_w, frame_h, false).unwrap();
    let px = lay.paimon_rect.x;
    let py = lay.paimon_rect.y;
    let pw = lay.paimon_rect.width as usize;
    let ph = lay.paimon_rect.height as usize;
    let mx = lay.minimap_rect.x;
    let my = lay.minimap_rect.y;
    let mw = lay.minimap_rect.width as usize;
    let mh = lay.minimap_rect.height as usize;
    let pad_x = (48.0 * lay.scale_ui).round_ties_even() as usize;
    let pad_y = (28.0 * lay.scale_ui).round_ties_even() as usize;
    let hud_w = (mx as usize + mw + pad_x).max((lay.paimon_maybe_rect.x + lay.paimon_maybe_rect.width) as usize);
    let hud_h = (my as usize + mh + pad_y).max((lay.paimon_maybe_rect.y + lay.paimon_maybe_rect.height) as usize);

    let mut state = 77u64;
    let mut hud = Array3::<u8>::zeros((hud_h, hud_w, 3));
    for v in hud.iter_mut() {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        *v = (14 + ((state >> 24) % 20)) as u8;
    }

    let mm_bgr = minimap.slice(s![.., .., 0..3]);
    let mm_r = if mm_bgr.dim().0 == mh && mm_bgr.dim().1 == mw {
        mm_bgr.to_owned()
    } else {
        resize_u8_hwc(mm_bgr, mw, mh, Interpolation::Area).unwrap()
    };
    let mx0 = (mx + dx_px).clamp(0, (hud_w - mw) as i32) as usize;
    let my0 = (my + dy_px).clamp(0, (hud_h - mh) as i32) as usize;
    hud.slice_mut(s![my0..my0 + mh, mx0..mx0 + mw, ..])
        .assign(&mm_r);

    if let Some(b) = badge {
        let br = if b.dim().0 == ph && b.dim().1 == pw {
            b.clone()
        } else {
            resize_u8_hwc(b.view(), pw, ph, Interpolation::Linear).unwrap()
        };
        let px0 = (px + dx_px).clamp(0, (hud_w - pw) as i32) as usize;
        let py0 = (py + dy_px).clamp(0, (hud_h - ph) as i32) as usize;
        hud.slice_mut(s![py0..py0 + ph, px0..px0 + pw, ..])
            .assign(&br);
    }
    hud
}

fn make_synthetic_ug_overlay_png(width: usize, height: usize) -> Vec<u8> {
    let mut img = Array3::<u8>::zeros((height, width, 4));
    let mut state = 31415u64;
    let mut next_u64 = || -> u64 {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        state >> 16
    };
    for y in 0..height {
        for x in 0..width {
            let n = (next_u64() % 13) as i32 - 6;
            img[[y, x, 0]] = (36 + n).clamp(0, 255) as u8;
            img[[y, x, 1]] = (40 + n).clamp(0, 255) as u8;
            img[[y, x, 2]] = (48 + n).clamp(0, 255) as u8;
            img[[y, x, 3]] = 255;
        }
    }
    for idx in 0..45usize {
        let x0 = 40 + (next_u64() as usize % (width - 100));
        let y0 = 40 + (next_u64() as usize % (height - 100));
        let r0 = 10 + (next_u64() as usize % 28);
        let col = (130 + (idx % 5) * 24) as u8;
        for dy in 0..r0 {
            for dx in 0..(r0 + 12) {
                if dy < 3 || dy + 3 >= r0 || dx < 3 || dx + 3 >= r0 + 12 {
                    img[[y0 + dy, x0 + dx, 0]] = col;
                    img[[y0 + dy, x0 + dx, 1]] = col.saturating_sub(10);
                    img[[y0 + dy, x0 + dx, 2]] = col.saturating_sub(20);
                }
            }
        }
        let cx = (x0 + r0 / 2) as isize;
        let cy = (y0 + r0 / 2) as isize;
        let rad = (r0 / 4).max(4) as isize;
        for dy in -rad..=rad {
            for dx in -rad..=rad {
                if dx * dx + dy * dy <= rad * rad {
                    let yy = (cy + dy) as usize;
                    let xx = (cx + dx) as usize;
                    img[[yy, xx, 0]] = 210;
                    img[[yy, xx, 1]] = 200;
                    img[[yy, xx, 2]] = 175;
                }
            }
        }
    }

    let rgba = ImageBuffer::from_fn(width as u32, height as u32, |x, y| {
        let (x, y) = (x as usize, y as usize);
        Rgba([img[[y, x, 2]], img[[y, x, 1]], img[[y, x, 0]], img[[y, x, 3]]])
    });
    let mut out = Vec::new();
    rgba.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn median_f64(vals: &mut [f64]) -> f64 {
    assert!(!vals.is_empty());
    vals.sort_by(|a, b| a.total_cmp(b));
    let n = vals.len();
    if n % 2 == 1 {
        vals[n / 2]
    } else {
        0.5 * (vals[n / 2 - 1] + vals[n / 2])
    }
}

fn percentile_f64(vals: &mut [f64], p: f64) -> f64 {
    assert!(!vals.is_empty());
    vals.sort_by(|a, b| a.total_cmp(b));
    let idx = ((p * (vals.len() - 1) as f64).round_ties_even() as usize).min(vals.len() - 1);
    vals[idx]
}

struct MapUgSource {
    entries: HashMap<(i32, String), Vec<u8>>,
}

impl UndergroundOverlaySource for MapUgSource {
    fn get_overlay_bytes(&self, map_id: i32, floor_id: &str, _async_fetch: bool) -> Option<Vec<u8>> {
        self.entries.get(&(map_id, floor_id.to_owned())).cloned()
    }
}

#[test]
#[ignore]
fn e2_verify_minimap_tracker_v5_parity() {
    let root = repo_root();
    let npz_path = terrain_npz_path(&root);
    let Some(map2_raster) = load_map2_raster(&root) else {
        eprintln!("SKIP e2_verify_minimap_tracker_v5_parity: terrain_n2_v4.npz missing");
        return;
    };
    let tmp = TempDirGuard::new("gh_e2_v5");

    // --- Test (a): LruTerrainTileCache max_tiles=16, real NPZ slice, disk cache, underground Y-flip ---
    let cache = LruTerrainTileCache::from_config(TileCacheConfig {
        max_tiles: 16,
        tile_size: 256,
        npz_path: Some(npz_path.clone()),
        use_default_npz: false,
        disk_cache_dir: Some(tmp.0.join("tiles_a")),
        terrain_data: BTreeMap::new(),
        tile_provider: None,
        async_fetch: false,
    })
    .unwrap();

    let mut rng_s = 20261121u64;
    let mut loaded_count = 0usize;
    for i in 0..60 {
        rng_s = rng_s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let tx = 7 + ((rng_s >> 24) % 22) as i32;
        rng_s = rng_s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let ty = 6 + ((rng_s >> 24) % 20) as i32;
        let tile = cache.get_tile(2, -2, tx, ty);
        assert!(cache.tile_count() <= 16, "exceeded 16 at {i}");
        if let Some(t) = tile {
            assert_eq!(t.dim(), (256, 256));
            loaded_count += 1;
        }
    }
    assert!(loaded_count >= 50, "loaded_count={loaded_count}");
    assert!(cache.evictions() > 0);
    assert!(cache.approx_ram_bytes() <= 16 * 256 * 256);

    let [tx0, ty0, z_nat, orig_x, orig_y] = map2_raster.meta;
    let pow2z = 2.0f64.powf(z_nat);
    for (label, wx, wy) in [("MondstadtCity", -503.0, -1267.0), ("LiyueHarbor", -1103.0, 2101.0)] {
        let (p_opt, meta) = cache.extract_local_patch(2, wx, wy, 200.0, 0.25);
        let patch = p_opt.expect(label);
        assert_eq!(patch.dim(), (100, 100));
        let (cx_w, cy_w) = meta.pixel_to_world(49.5, 49.5);
        assert!((cx_w - wx).abs() < 1e-6 && (cy_w - wy).abs() < 1e-6);
        let rx_f = (meta.origin_wx + orig_x) * pow2z - tx0 * 256.0;
        let ry_f = (meta.origin_wy + orig_y) * pow2z - ty0 * 256.0;
        let rx = rx_f.round_ties_even() as usize;
        let ry = ry_f.round_ties_even() as usize;
        let ref_crop = map2_raster.gray.slice(s![ry..ry + 100, rx..rx + 100]).mapv(|v| v as f32);
        let patch_f = patch.mapv(|v| v as f32);
        let ncc = match_template_f32(ref_crop.view(), patch_f.view(), TemplateMatchMode::CcoeffNormed)
            .unwrap();
        let (_, corr, _, _) = min_max_loc_f32(ncc.view()).unwrap();
        assert!(corr > 0.999, "{label} NCC={corr}");
    }

    // Disk cache precedence
    let disk_dir = tmp.0.join("tiles_disk");
    write_image_gray(
        &disk_dir.join("2").join("-1").join("45_30.png"),
        Array2::<u8>::from_elem((256, 256), 173).view(),
        None,
    )
    .unwrap();
    let disk_cache = LruTerrainTileCache::from_config(TileCacheConfig {
        max_tiles: 16,
        tile_size: 256,
        npz_path: Some(npz_path.clone()),
        use_default_npz: false,
        disk_cache_dir: Some(disk_dir),
        terrain_data: BTreeMap::new(),
        tile_provider: None,
        async_fetch: false,
    })
    .unwrap();
    assert_eq!(disk_cache.get_tile(2, -1, 45, 30).unwrap()[[128, 128]], 173);

    // Underground Y-flip 4a/4b/4c
    let mut ug_north_up = Array2::<u8>::from_elem((200, 200), 40);
    ug_north_up.slice_mut(s![20..55, 70..130]).fill(235);
    ug_north_up.slice_mut(s![150..180, 70..130]).fill(95);
    let b_std = OverlayBounds::new(-4000.0, -7400.0, -3200.0, -6600.0);
    let (p_std, _) =
        extract_underground_patch_gray(ug_north_up.view(), &b_std, -3600.0, -7000.0, 400.0, 0.25, None);
    let p_std = p_std.unwrap();
    let b_opp = OverlayBounds::new(-4000.0, 7400.0, -3200.0, 6600.0);
    let (mat_opp, refl) =
        compose_underground_local_to_surface(&b_opp, 200, 200, Some(-7000.0), 400.0, None).unwrap();
    assert!(refl && mat_opp[1][1] > 0.0);
    let (p_opp, _) =
        extract_underground_patch_gray(ug_north_up.view(), &b_opp, -3600.0, -7000.0, 400.0, 0.25, None);
    let p_opp = p_opp.unwrap();
    let diff_opp: f64 = p_std
        .iter()
        .zip(p_opp.iter())
        .map(|(&a, &b)| a.abs_diff(b) as f64)
        .sum::<f64>()
        / p_std.len() as f64;
    assert!(diff_opp < 1.0, "diff_opp={diff_opp}");

    // --- Test (b): 25-step Mondstadt trajectory, stationary skip < 1ms, Liyue BGR, 2-zoom hysteresis ---
    let mut terrain = BTreeMap::new();
    terrain.insert(2, Arc::clone(&map2_raster));
    let shared_cache = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: Some(npz_path.clone()),
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles_b")),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap(),
    );
    let mut tracker =
        HybridMinimapTracker::new(Arc::clone(&shared_cache), MinimapTrackerConfig::default())
            .unwrap();
    let (wx0, wy0) = (-500.0, -1266.0);
    let mut t = 100.0;
    let ap = tracker
        .set_map_m_anchor(2, wx0, wy0, false, None, "G", Some(t))
        .unwrap();
    assert_eq!(ap.state, TrackerState::Tracking);

    let mut max_err = 0.0f64;
    let mut last_mm = None;
    for step_idx in 1..=25 {
        t += 0.1;
        let tx = wx0 + step_idx as f64 * 1.1;
        let ty = wy0 + step_idx as f64 * 0.8;
        let cam_ang = wrap_angle_360(35.0 + step_idx as f64 * 6.0);
        let body_ang = wrap_angle_360(125.0 + step_idx as f64 * 4.0);
        let mm = render_synthetic_minimap(
            &shared_cache,
            2,
            tx,
            ty,
            360.0,
            218,
            cam_ang,
            body_ang,
            false,
            true,
            None,
        );
        let pose = tracker.step_array3(Some(mm.view()), Some(t), None).unwrap();
        assert_eq!(pose.state, TrackerState::Tracking);
        let err = (pose.world_x.unwrap() - tx).hypot(pose.world_y.unwrap() - ty);
        max_err = max_err.max(err);
        last_mm = Some(mm);
    }
    assert!(max_err < 3.0, "25-step max_err={max_err}");

    let last_frame = last_mm.unwrap();
    t += 0.1;
    let calls_before = tracker.matcher_calls();
    let p_s0 = tracker
        .step_array3(Some(last_frame.view()), Some(t), None)
        .unwrap();
    assert!(p_s0.stationary);
    assert_eq!(tracker.matcher_calls(), calls_before);

    let mut stat_ms = Vec::new();
    for _ in 0..15 {
        t += 0.1;
        let st0 = Instant::now();
        let ps = tracker
            .step_array3(Some(last_frame.view()), Some(t), None)
            .unwrap();
        stat_ms.push(st0.elapsed().as_secs_f64() * 1000.0);
        assert!(ps.stationary);
    }
    assert_eq!(tracker.matcher_calls(), calls_before);
    let med_stat = median_f64(&mut stat_ms);
    assert!(med_stat < 1.0, "stationary median={med_stat}ms");

    // Rotating only arrow keeps stationary=true
    t += 0.1;
    let mm_rot_body = render_synthetic_minimap(
        &shared_cache,
        2,
        wx0 + 25.0 * 1.1,
        wy0 + 25.0 * 0.8,
        360.0,
        218,
        wrap_angle_360(35.0 + 25.0 * 6.0),
        15.0,
        false,
        true,
        None,
    );
    let cb_rot = tracker.matcher_calls();
    let p_rb = tracker
        .step_array3(Some(mm_rot_body.view()), Some(t), None)
        .unwrap();
    assert!(p_rb.stationary && tracker.matcher_calls() == cb_rot);

    // Rotating camera while stationary updates camera_heading
    t += 0.1;
    let mm_rot_cam = render_synthetic_minimap(
        &shared_cache,
        2,
        wx0 + 25.0 * 1.1,
        wy0 + 25.0 * 0.8,
        360.0,
        218,
        260.0,
        10.0,
        false,
        true,
        None,
    );
    let p_rc = tracker
        .step_array3(Some(mm_rot_cam.view()), Some(t), None)
        .unwrap();
    assert!(shortest_angle_delta(260.0, p_rc.camera_heading.unwrap()).abs() < 45.0);

    // Liyue 3-ch BGR
    let (lyx, lyy) = (-1100.0, 2100.0);
    t += 0.1;
    tracker
        .set_map_m_anchor(2, lyx, lyy, false, None, "G", Some(t))
        .unwrap();
    let mut ly_max_err = 0.0f64;
    for s_idx in 1..=10 {
        t += 0.1;
        let tx = lyx + s_idx as f64;
        let ty = lyy - s_idx as f64 * 0.9;
        let mm_bgr = render_synthetic_minimap(
            &shared_cache,
            2,
            tx,
            ty,
            360.0,
            218,
            120.0,
            120.0,
            false,
            false,
            None,
        );
        let p_ly = tracker
            .step_array3(Some(mm_bgr.view()), Some(t), None)
            .unwrap();
        assert_eq!(p_ly.state, TrackerState::Tracking);
        ly_max_err = ly_max_err.max((p_ly.world_x.unwrap() - tx).hypot(p_ly.world_y.unwrap() - ty));
    }
    assert!(ly_max_err < 3.0, "Liyue BGR max_err={ly_max_err}");

    // 2-zoom hysteresis
    let (cx, cy) = (-480.0, -1245.0);
    t += 0.1;
    tracker
        .set_map_m_anchor(2, cx, cy, false, None, "G", Some(t))
        .unwrap();
    let mm_open = render_synthetic_minimap(
        &shared_cache, 2, cx, cy, 360.0, 218, 60.0, 60.0, false, true, None,
    );
    let mm_c1 = render_synthetic_minimap(
        &shared_cache, 2, cx + 1.0, cy + 1.0, 120.0, 218, 60.0, 60.0, false, true, None,
    );
    let mm_c2 = render_synthetic_minimap(
        &shared_cache, 2, cx + 2.0, cy + 2.0, 120.0, 218, 60.0, 60.0, false, true, None,
    );
    t += 0.1;
    assert_eq!(
        tracker.step_array3(Some(mm_open.view()), Some(t), None).unwrap().zoom_regime,
        ZoomRegime::OpenWorld
    );
    t += 0.1;
    assert_eq!(
        tracker.step_array3(Some(mm_c1.view()), Some(t), None).unwrap().zoom_regime,
        ZoomRegime::OpenWorld
    );
    t += 0.1;
    assert_eq!(
        tracker.step_array3(Some(mm_open.view()), Some(t), None).unwrap().zoom_regime,
        ZoomRegime::OpenWorld
    );
    t += 0.1;
    assert_eq!(
        tracker.step_array3(Some(mm_c1.view()), Some(t), None).unwrap().zoom_regime,
        ZoomRegime::OpenWorld
    );
    t += 0.1;
    assert_eq!(
        tracker.step_array3(Some(mm_c2.view()), Some(t), None).unwrap().zoom_regime,
        ZoomRegime::City
    );

    // --- Test (c): Follow-View ORB rotation recovery across 6 headings + return to LockDirection ---
    let mut fv_tracker =
        HybridMinimapTracker::new(Arc::clone(&shared_cache), MinimapTrackerConfig::default())
            .unwrap();
    let mut tfv = 200.0;
    fv_tracker
        .set_map_m_anchor(2, wx0, wy0, false, None, "G", Some(tfv))
        .unwrap();
    let mut max_fv_err = 0.0f64;
    for (idx, h_deg) in [35.0, 90.0, 145.0, 225.0, 270.0, 315.0].iter().enumerate() {
        tfv += 0.1;
        let mm_fv = render_synthetic_minimap(
            &shared_cache,
            2,
            wx0 + idx as f64 * 1.5,
            wy0 + idx as f64 * 1.0,
            360.0,
            218,
            *h_deg,
            *h_deg,
            true,
            true,
            None,
        );
        let p_fv = fv_tracker
            .step_array3(Some(mm_fv.view()), Some(tfv), None)
            .unwrap();
        assert_eq!(p_fv.state, TrackerState::Tracking);
        assert_eq!(p_fv.minimap_mode, MinimapMode::FollowView);
        let a_err = shortest_angle_delta(*h_deg, p_fv.view_heading.unwrap()).abs();
        max_fv_err = max_fv_err.max(a_err);
        assert!(a_err < 6.0, "Follow-View heading {h_deg} err={a_err}");
    }
    tfv += 0.1;
    let mm_lock = render_synthetic_minimap(
        &shared_cache,
        2,
        wx0 + 9.0,
        wy0 + 6.0,
        360.0,
        218,
        130.0,
        130.0,
        false,
        true,
        None,
    );
    let p_lock = fv_tracker
        .step_array3(Some(mm_lock.view()), Some(tfv), None)
        .unwrap();
    assert_eq!(p_lock.minimap_mode, MinimapMode::LockDirection);

    // --- Test (e): Multi-layer surface vs underground comparison ---
    let mut layer_tr =
        HybridMinimapTracker::new(Arc::clone(&shared_cache), MinimapTrackerConfig::default())
            .unwrap();
    layer_tr
        .set_map_m_anchor(2, wx0, wy0, false, None, "G", Some(400.0))
        .unwrap();
    let (surf_patch_opt, _) = shared_cache.extract_local_patch(2, wx0, wy0, 180.0, 0.5);
    let surf_patch = surf_patch_opt.unwrap();
    let (ph, pw) = surf_patch.dim();
    let mut ug_patch = Array2::<u8>::from_elem((ph, pw), 28);
    for d in 0..ph.min(pw) {
        for off in -3isize..=3 {
            let yy = (d as isize + off).clamp(0, ph as isize - 1) as usize;
            ug_patch[[yy, d]] = 210;
            ug_patch[[yy, pw - 1 - d]] = 195;
        }
    }
    draw_filled_circle_u8(&mut ug_patch, (pw / 2) as i32, (ph / 3) as i32, 22, 230);
    let mm_surf = render_minimap_from_gray(surf_patch.view(), 218, 45.0, 45.0, false, false);
    let mm_ug = render_minimap_from_gray(ug_patch.view(), 218, 45.0, 45.0, false, false);
    let mm_surf_g = bgr_to_gray(mm_surf.view()).unwrap();
    let mm_ug_g = bgr_to_gray(mm_ug.view()).unwrap();

    let r1 = layer_tr.compare_surface_vs_underground_gray(
        mm_ug_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
    );
    assert!(r1.margin >= 0.08 && !r1.is_underground && r1.streak == 1);
    let r2 = layer_tr.compare_surface_vs_underground_gray(
        mm_ug_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
    );
    assert!(r2.margin >= 0.08 && !r2.is_underground && r2.streak == 2);
    let r_rst = layer_tr.compare_surface_vs_underground_gray(
        mm_surf_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
    );
    assert!(!r_rst.is_underground && r_rst.streak == 0);

    let mut blended = Array2::<u8>::zeros((ph, pw));
    for y in 0..ph {
        for x in 0..pw {
            blended[[y, x]] = ((surf_patch[[y, x]] as u16 + ug_patch[[y, x]] as u16) / 2) as u8;
        }
    }
    let mm_amb = render_minimap_from_gray(blended.view(), 218, 45.0, 45.0, false, false);
    let mm_amb_g = bgr_to_gray(mm_amb.view()).unwrap();
    for _ in 0..5 {
        let ra = layer_tr.compare_surface_vs_underground_gray(
            mm_amb_g.view(), Some(blended.view()), Some(blended.view()), Some(78), "B1", None, true,
        );
        assert!(ra.margin.abs() < 0.01 && !ra.is_underground);
    }

    for _ in 0..2 {
        let u = layer_tr.compare_surface_vs_underground_gray(
            mm_ug_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
        );
        assert!(!u.is_underground);
    }
    let u3 = layer_tr.compare_surface_vs_underground_gray(
        mm_ug_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
    );
    assert!(u3.switched && u3.is_underground && u3.floor_id == Some(78));

    for _ in 0..2 {
        let s = layer_tr.compare_surface_vs_underground_gray(
            mm_surf_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
        );
        assert!(s.is_underground);
    }
    let s3 = layer_tr.compare_surface_vs_underground_gray(
        mm_surf_g.view(), Some(surf_patch.view()), Some(ug_patch.view()), Some(78), "B1", None, true,
    );
    assert!(s3.switched && !s3.is_underground && s3.floor_id.is_none());

    // --- Test (f): LocalizationWorker live mailbox + visible_snapshot + shutdown ---
    let w_tr =
        HybridMinimapTracker::new(Arc::clone(&shared_cache), MinimapTrackerConfig::default())
            .unwrap();
    let mm_w = render_synthetic_minimap(
        &shared_cache, 2, wx0, wy0, 360.0, 218, 90.0, 90.0, false, true, None,
    );
    let mut mm_fb = FrameBuf::new_zeroed(218, 218, PixelFormat::Bgra8);
    mm_fb.data.copy_from_slice(mm_w.as_slice().unwrap());
    let cap_n = Arc::new(AtomicUsize::new(0));
    let cap_n_c = Arc::clone(&cap_n);
    let mut worker = LocalizationWorker::new(
        w_tr,
        Box::new(move || {
            cap_n_c.fetch_add(1, Ordering::Relaxed);
            Some(mm_fb.clone())
        }),
        LocalizationScheduler::new(0.02, 0.05, 0.5, 1.45),
        1.5,
        Duration::from_millis(5),
        None,
    );
    worker.anchor(2, wx0, wy0, false, None, "G");
    let dl = Instant::now() + Duration::from_secs(2);
    let mut w_latest = None;
    while Instant::now() < dl {
        if let Ok(Some(lr)) = worker.latest()
            && lr.pose.state == TrackerState::Tracking
            && cap_n.load(Ordering::Relaxed) >= 1
        {
            w_latest = Some(lr);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let lr = w_latest.expect("worker did not publish");
    assert_eq!(
        visible_snapshot(Some(&lr), Some(lr.captured_at + 0.3), 1.5).unwrap().state,
        TrackerState::Tracking
    );
    let exp = visible_snapshot(Some(&lr), Some(lr.captured_at + 2.0), 1.5).unwrap();
    assert_eq!(exp.state, TrackerState::Lost);
    assert_eq!(exp.reason.as_deref(), Some("capture_expired"));
    worker.stop();
    assert!(!worker.is_running());

    // --- Test (g): 360-deg camera spin zero drift + real B1 underground fixture ---
    let mut spin_tr =
        HybridMinimapTracker::new(Arc::clone(&shared_cache), MinimapTrackerConfig::default())
            .unwrap();
    let mut tsp = 500.0;
    spin_tr
        .set_map_m_anchor(2, wx0, wy0, false, None, "G", Some(tsp))
        .unwrap();
    tsp += 0.1;
    let mm_sp0 = render_synthetic_minimap(
        &shared_cache, 2, wx0, wy0, 360.0, 218, 0.0, 0.0, false, false, None,
    );
    assert_eq!(
        spin_tr.step_array3(Some(mm_sp0.view()), Some(tsp), None).unwrap().state,
        TrackerState::Tracking
    );
    for cam in [45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0, 0.0] {
        tsp += 0.1;
        let mms = render_synthetic_minimap(
            &shared_cache, 2, wx0, wy0, 360.0, 218, cam, 0.0, false, false, None,
        );
        let ps = spin_tr.step_array3(Some(mms.view()), Some(tsp), None).unwrap();
        assert_eq!(ps.state, TrackerState::Tracking);
        assert!(ps.stationary, "spin at {cam} deg drifted from stationary");
        assert!((ps.world_x.unwrap() - wx0).hypot(ps.world_y.unwrap() - wy0) < 3.0);
    }

    let ug_fix = root
        .join("scratch")
        .join("diagnostics")
        .join("diag_20261121_205434")
        .join("01_game.png");
    if ug_fix.is_file() {
        let game_img = read_image_bgr(&ug_fix).unwrap();
        let (gh, gw, _) = game_img.dim();
        let lay = compute_ui_layout(gw as u32, gh as u32, false).unwrap();
        let mr = lay.minimap_rect;
        let mm_real = game_img.slice(s![
            mr.y as usize..(mr.y + mr.height) as usize,
            mr.x as usize..(mr.x + mr.width) as usize,
            ..
        ]);
        let (ug_x, ug_y) = (-5071.2, -1638.5);
        tsp += 0.1;
        spin_tr
            .set_map_m_anchor(2, ug_x, ug_y, true, Some(78), "B1", Some(tsp))
            .unwrap();
        for _ in 0..10 {
            tsp += 0.1;
            let pu = spin_tr
                .step_array3(Some(mm_real), Some(tsp), None)
                .unwrap();
            assert_eq!(pu.state, TrackerState::Tracking);
            assert!(pu.is_underground && pu.floor_id == Some(78));
            assert!((pu.world_x.unwrap() - ug_x).hypot(pu.world_y.unwrap() - ug_y) < 5.0);
        }
    }

    eprintln!(
        "e2_verify_minimap_tracker_v5_parity PASS (25-step max_err={max_err:.2}w, stat_med={med_stat:.3}ms, fv_max_err={max_fv_err:.2}deg)"
    );
}

#[test]
#[ignore]
fn e2_verify_v53_production_parity() {
    let root = repo_root();
    let npz_path = terrain_npz_path(&root);
    let mm_path = multimap_json_path(&root);
    let Some(map2_raster) = load_map2_raster(&root) else {
        eprintln!("SKIP e2_verify_v53_production_parity: terrain_n2_v4.npz missing");
        return;
    };
    if !mm_path.is_file() {
        eprintln!("SKIP e2_verify_v53_production_parity: multimap_v4.json missing");
        return;
    }
    let db = Arc::new(load_multimap_db(&mm_path).unwrap());
    let tmp = TempDirGuard::new("gh_e2_v53");
    let idx_path = tmp.0.join(GLOBAL_INDEX_FILENAME);

    let mut terrain = BTreeMap::new();
    terrain.insert(2, Arc::clone(&map2_raster));
    let tc = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: LRU_TILE_CAPACITY,
            tile_size: 256,
            npz_path: Some(npz_path),
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap(),
    );

    // --- Test (c): TeyvatGlobalRelocator v3 warmup, recall, negatives, warm reload ---
    let reloc_lazy =
        TeyvatGlobalRelocator::new(Some(Arc::clone(&map2_raster)), Some(idx_path.clone()), true);
    assert!(!reloc_lazy.is_ready());
    reloc_lazy.warmup_async();
    let dl = Instant::now() + Duration::from_secs(30);
    while !reloc_lazy.is_ready() && Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(reloc_lazy.is_ready(), "warmup_async timed out");
    assert!(idx_path.is_file());
    let npz_check = NpzArchive::read_file(&idx_path).unwrap();
    let NpyData::I32(ver) = &npz_check.get("index_version").unwrap().data else {
        panic!("missing index_version");
    };
    assert_eq!(ver[0], INDEX_VERSION);

    let (wx, wy) = (-500.0, -1266.0);
    let mm_mond = render_synthetic_minimap(&tc, 2, wx, wy, 360.0, 218, 45.0, 45.0, false, true, None);
    let mut vbins = HashMap::new();
    let cands = reloc_lazy.query_candidates(mm_mond.view(), Some(&mut vbins), 4, 1.5);
    assert!(!cands.is_empty(), "global_relocator returned 0 candidates");
    let best_dist = cands
        .iter()
        .map(|c| (c.world_x - wx).hypot(c.world_y - wy))
        .fold(f64::INFINITY, f64::min);
    assert!(best_dist <= 250.0, "best_dist={best_dist}");

    for fill in [0u8, 248, 90] {
        let neg = Array3::<u8>::from_elem((218, 218, 3), fill);
        assert!(reloc_lazy.query_candidates(neg.view(), None, 4, 3.5).is_empty());
    }

    let t_reload = Instant::now();
    let reloc = Arc::new(TeyvatGlobalRelocator::new(
        Some(Arc::clone(&map2_raster)),
        Some(idx_path),
        false,
    ));
    let reload_s = t_reload.elapsed().as_secs_f64();
    assert!(reloc.is_ready() && reload_s < 2.0, "reload_s={reload_s}");

    // --- Test (d): Paimon auto-bootstrap, HUD cal, pause, bidirectional auto-layer, cold + Tier-3 ---
    let (badge_bgr, badge_mask) = make_synthetic_badge(68, 77);
    let paimon_cache = tmp.0.join("paimon_tpl.npz");
    let mut tr_boot =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::production_defaults())
            .unwrap()
            .with_global_relocator(Arc::clone(&reloc))
            .with_paimon_cache_path(paimon_cache.clone());
    let mut t = 10.0;
    tr_boot
        .set_map_m_anchor(2, wx, wy, false, None, "G", Some(t))
        .unwrap();
    for i in 1..=5 {
        t += 0.2;
        let mm = render_synthetic_minimap(
            &tc,
            2,
            wx + i as f64 * 4.5,
            wy + i as f64 * 3.0,
            360.0,
            218,
            45.0,
            45.0,
            false,
            true,
            None,
        );
        let hud = compose_hud_frame(&mm, Some(&badge_bgr), 1920, 1080, 0, 0);
        let pose = tr_boot
            .step_from_hud_array3(Some(hud.view()), 1920, 1080, Some(t), false, None)
            .unwrap();
        assert_eq!(pose.state, TrackerState::Tracking);
    }
    assert!(paimon_cache.is_file() && tr_boot.has_paimon_tpl());

    t += 0.2;
    let mm_now = render_synthetic_minimap(
        &tc,
        2,
        wx + 22.0,
        wy + 15.0,
        360.0,
        218,
        45.0,
        45.0,
        false,
        true,
        None,
    );
    let hud_no_pm = compose_hud_frame(&mm_now, None, 1920, 1080, 0, 0);
    let p_pause = tr_boot
        .step_from_hud_array3(Some(hud_no_pm.view()), 1920, 1080, Some(t), false, None)
        .unwrap();
    assert_eq!(p_pause.match_method, "paimon_pause");
    assert!(!p_pause.should_broadcast_web);

    // HUD offset auto-calibration (dx=-12, dy=6)
    let mut tr_cal =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::production_defaults())
            .unwrap()
            .with_paimon_template(badge_bgr.view(), badge_mask.view());
    let mut t2 = 50.0;
    tr_cal
        .set_map_m_anchor(2, wx, wy, false, None, "G", Some(t2))
        .unwrap();
    let mut hud_last = None;
    let mut pose_cal = None;
    for k in 1..=5 {
        t2 += 0.2;
        let mms = render_synthetic_minimap(
            &tc,
            2,
            wx + k as f64 * 4.5,
            wy + k as f64 * 3.0,
            360.0,
            218,
            45.0,
            45.0,
            false,
            true,
            None,
        );
        let hud = compose_hud_frame(&mms, Some(&badge_bgr), 1920, 1080, -12, 6);
        pose_cal = Some(
            tr_cal
                .step_from_hud_array3(Some(hud.view()), 1920, 1080, Some(t2), false, None)
                .unwrap(),
        );
        hud_last = Some(hud);
    }
    let (cdx, cdy) = tr_cal.calibrated_offset_px();
    assert!((cdx - (-12)).abs() <= 2 && (cdy - 6).abs() <= 2, "cal=({cdx},{cdy})");
    let pc = pose_cal.unwrap();
    assert!(pc.state == TrackerState::Tracking && pc.should_broadcast_web);
    let err_cal = (pc.world_x.unwrap() - (wx + 22.5)).hypot(pc.world_y.unwrap() - (wy + 15.0));
    assert!(err_cal < 8.0, "err_cal={err_cal}");

    t2 += 0.2;
    let p_stat = tr_cal
        .step_from_hud_array3(hud_last.as_ref().map(|h| h.view()), 1920, 1080, Some(t2), false, None)
        .unwrap();
    assert_eq!(p_stat.match_method, "stationary_skip");
    assert!(!p_stat.should_broadcast_web);

    t2 += 0.2;
    let p_load = tr_cal.on_loading_screen(Some(t2)).unwrap();
    assert_eq!(p_load.match_method, "loading_pause");
    assert!(!p_load.should_broadcast_web);

    // Bidirectional Auto-Layer-Switch (Surf -> UG -> Surf)
    let mut chosen = None;
    for rec in db.floor_overlays_for_map(2) {
        if (rec.rx - rec.lx) >= 360.0 && (rec.ry - rec.ly) >= 360.0 {
            let cx = 0.5 * (rec.lx + rec.rx);
            let cy = 0.5 * (rec.ly + rec.ry);
            if let (Some(sp), _) = tc.extract_local_patch(2, cx, cy, 180.0, 0.5) {
                let n = sp.len() as f64;
                let m = sp.iter().map(|&v| v as f64).sum::<f64>() / n;
                let std = (sp.iter().map(|&v| (v as f64 - m).powi(2)).sum::<f64>() / n).sqrt();
                if std >= 20.0 {
                    chosen = Some(rec);
                    break;
                }
            }
        }
    }
    let rec = chosen.expect("no textured Map 2 underground floor found");
    let ug_png = make_synthetic_ug_overlay_png(512, 512);
    let decoded_ug = gh_cv::image_io::decode_image_bytes(&ug_png, ImageReadMode::Unchanged).unwrap();
    let DecodedImage::Bgra(ug_arr) = decoded_ug else {
        panic!("expected BGRA");
    };
    let ug_gray_full = bgr_to_gray(ug_arr.view()).unwrap();
    let ug_bounds = OverlayBounds::from_floor_overlay_meta(&rec.overlay);

    let mut entries = HashMap::new();
    entries.insert((2, format!("{}:{}", rec.group_id, rec.floor_id)), ug_png);
    let ug_src: Arc<dyn UndergroundOverlaySource> = Arc::new(MapUgSource { entries });

    let mut tr_layer =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::production_defaults())
            .unwrap()
            .with_multimap_db(Arc::clone(&db))
            .with_ug_provider(ug_src);

    let fl_cx = 0.5 * (rec.lx + rec.rx);
    let fl_cy = 0.5 * (rec.ly + rec.ry);
    let mut t3 = 100.0;
    tr_layer
        .set_map_m_anchor(2, fl_cx, fl_cy, false, None, "G", Some(t3))
        .unwrap();

    let mut switched_to_ug = false;
    let mut cur_wx = fl_cx;
    let mut cur_wy = fl_cy;
    for k in 1..=8 {
        t3 += 0.2;
        cur_wx = fl_cx + k as f64 * 4.5;
        cur_wy = fl_cy + k as f64 * 3.5;
        let (ug_p_opt, _) = extract_underground_patch_gray(
            ug_gray_full.view(),
            &ug_bounds,
            cur_wx,
            cur_wy,
            180.0 * 1.02,
            218.0 / 360.0,
            None,
        );
        let ug_p = ug_p_opt.unwrap();
        let mm_ug = render_minimap_from_gray(
            ug_p.view(),
            218,
            60.0 + k as f64 * 5.0,
            60.0 + k as f64 * 5.0,
            false,
            true,
        );
        let pl = tr_layer
            .step_array3(Some(mm_ug.view()), Some(t3), None)
            .unwrap();
        if pl.match_method == "layer_switch_sift" && pl.is_underground {
            switched_to_ug = true;
            assert_eq!(pl.floor_id, Some(rec.floor_id));
            break;
        }
    }
    assert!(switched_to_ug, "Auto-layer switch Surf -> UG did not trigger");

    let mut switched_to_surf = false;
    for k in 1..=8 {
        t3 += 0.2;
        let mm_sf = render_synthetic_minimap(
            &tc,
            2,
            cur_wx + k as f64 * 4.5,
            cur_wy + k as f64 * 3.5,
            360.0,
            218,
            120.0 + k as f64 * 5.0,
            120.0 + k as f64 * 5.0,
            false,
            true,
            None,
        );
        let ps = tr_layer
            .step_array3(Some(mm_sf.view()), Some(t3), None)
            .unwrap();
        if ps.match_method == "layer_switch_sift" && !ps.is_underground {
            switched_to_surf = true;
            break;
        }
    }
    assert!(switched_to_surf, "Auto-layer switch UG -> Surf did not trigger");

    // Cold-start global_teyvat_relocate
    let mut tr_cold =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::production_defaults())
            .unwrap()
            .with_global_relocator(Arc::clone(&reloc));
    let p_cold = tr_cold
        .step_array3(Some(mm_mond.view()), Some(1.0), None)
        .unwrap();
    assert_eq!(p_cold.match_method, "global_teyvat_relocate");
    assert!(p_cold.state == TrackerState::Tracking && p_cold.should_broadcast_web);
    let cold_err = (p_cold.world_x.unwrap() - wx).hypot(p_cold.world_y.unwrap() - wy);
    assert!(cold_err < 20.0, "cold_err={cold_err}");

    // Tier-3 teleport recovery + checkerboard domain rejection
    let mut tr_tp =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::production_defaults())
            .unwrap()
            .with_global_relocator(Arc::clone(&reloc));
    let mut t4 = 200.0;
    tr_tp
        .set_map_m_anchor(2, -1800.0, 450.0, false, None, "G", Some(t4))
        .unwrap();
    t4 += 0.2;
    tr_tp.on_loading_screen(Some(t4)).unwrap();

    let mut cb = Array2::<u8>::zeros((218, 218));
    for by in 0..218 {
        for bx in 0..218 {
            if ((bx / 18) + (by / 18)) % 2 == 0 {
                cb[[by, bx]] = 195;
            }
        }
    }
    let mm_dom = render_minimap_from_gray(cb.view(), 218, 45.0, 45.0, false, true);
    for _ in 0..4 {
        t4 += 0.2;
        let p_dom = tr_tp
            .step_array3(Some(mm_dom.view()), Some(t4), None)
            .unwrap();
        assert!(
            p_dom.match_method == "relocating_hold" || p_dom.match_method == "gate_rejected",
            "unexpected method: {}",
            p_dom.match_method
        );
        assert!(!p_dom.should_broadcast_web);
        assert!((p_dom.world_x.unwrap() - (-1800.0)).hypot(p_dom.world_y.unwrap() - 450.0) < 1.0);
    }

    t4 += 0.2;
    let p_tp = tr_tp
        .step_array3(Some(mm_mond.view()), Some(t4), None)
        .unwrap();
    assert_eq!(p_tp.match_method, "global_teyvat_relocate");
    assert!(p_tp.should_broadcast_web);
    let tp_err = (p_tp.world_x.unwrap() - wx).hypot(p_tp.world_y.unwrap() - wy);
    assert!(tp_err < 20.0, "tp_err={tp_err}");

    eprintln!(
        "e2_verify_v53_production_parity PASS (best_dist={best_dist:.1}w, reload={reload_s:.3}s, cold={cold_err:.2}w, tier3={tp_err:.2}w)"
    );
}

#[test]
#[ignore]
fn e2_verify_nav_ux_v54_heading_fast_path() {
    let root = repo_root();
    let Some(map2_raster) = load_map2_raster(&root) else {
        eprintln!("SKIP e2_verify_nav_ux_v54_heading_fast_path: terrain_n2_v4.npz missing");
        return;
    };
    let tmp = TempDirGuard::new("gh_e2_v54");
    let mut terrain = BTreeMap::new();
    terrain.insert(2, Arc::clone(&map2_raster));
    let tc = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: Some(terrain_npz_path(&root)),
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap(),
    );
    let mut tr =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::default()).unwrap();
    let (wx, wy) = (-500.0, -1266.0);
    let mut t = 100.0;
    tr.set_map_m_anchor(2, wx, wy, false, None, "G", Some(t))
        .unwrap();
    t += 0.05;
    let mm0 = render_synthetic_minimap(&tc, 2, wx, wy, 360.0, 218, 40.0, 40.0, false, false, None);
    let full = tr.step_array3(Some(mm0.view()), Some(t), None).unwrap();
    assert_eq!(full.state, TrackerState::Tracking);

    let mut times_ms = Vec::new();
    let mut last_pose = None;
    for cam in [45.0, 50.0, 55.0, 60.0, 65.0, 70.0] {
        t += 1.0 / 60.0;
        let mm = render_synthetic_minimap(&tc, 2, wx, wy, 360.0, 218, cam, 40.0, false, false, None);
        let t0 = Instant::now();
        let pose = tr
            .step_headings_only_array3(Some(mm.view()), Some(t))
            .unwrap();
        times_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(pose.match_method, "heading_fast");
        assert_eq!(pose.state, TrackerState::Tracking);
        assert_eq!(pose.world_x, full.world_x);
        assert_eq!(pose.world_y, full.world_y);
        last_pose = Some(pose);
    }
    let lp = last_pose.unwrap();
    assert!(shortest_angle_delta(70.0, lp.camera_heading.unwrap()).abs() < 4.0);
    let med_ms = median_f64(&mut times_ms);
    assert!(med_ms < 8.0, "heading_fast median={med_ms}ms");

    let black = Array3::<u8>::zeros((218, 218, 3));
    assert!(
        tr.step_headings_only_array3(Some(black.view()), Some(t + 0.02))
            .is_none()
    );
    tr.pause(Some(t + 0.03), "map_m", 0.0).unwrap();
    let mm_ok = render_synthetic_minimap(&tc, 2, wx, wy, 360.0, 218, 70.0, 40.0, false, false, None);
    assert!(
        tr.step_headings_only_array3(Some(mm_ok.view()), Some(t + 0.04))
            .is_none()
    );
    let rect = tr.minimap_capture_rect(1920, 1080, false).unwrap();
    assert_eq!((rect.width, rect.height), (218, 218));
    assert!(rect.x >= 0 && rect.y >= 0);

    eprintln!("e2_verify_nav_ux_v54_heading_fast_path PASS (med_ms={med_ms:.3}ms)");
}

#[test]
#[ignore]
fn e2_rec_20261122_hud_replay_parity() {
    let root = repo_root();
    let rec_dir = root
        .join("scratch")
        .join("minimap_rec")
        .join("rec_20261122_043401");
    let meta_path = rec_dir.join("meta.jsonl");
    if !meta_path.is_file() {
        eprintln!("SKIP e2_rec_20261122_hud_replay_parity: rec_20261122_043401 missing");
        return;
    }
    let Some(map2_raster) = load_map2_raster(&root) else {
        eprintln!("SKIP e2_rec_20261122_hud_replay_parity: terrain_n2_v4.npz missing");
        return;
    };

    let tmp = TempDirGuard::new("gh_e2_rec");
    let idx_path = tmp.0.join(GLOBAL_INDEX_FILENAME);
    let paimon_cache = tmp.0.join("paimon_tpl.npz");

    let mut terrain = BTreeMap::new();
    terrain.insert(2, Arc::clone(&map2_raster));
    let tc = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: LRU_TILE_CAPACITY,
            tile_size: 256,
            npz_path: Some(terrain_npz_path(&root)),
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: terrain,
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap(),
    );
    let reloc = Arc::new(TeyvatGlobalRelocator::new(
        Some(Arc::clone(&map2_raster)),
        Some(idx_path),
        false,
    ));
    assert!(reloc.is_ready());

    let mut tracker =
        HybridMinimapTracker::new(Arc::clone(&tc), MinimapTrackerConfig::production_defaults())
            .unwrap()
            .with_global_relocator(reloc)
            .with_paimon_cache_path(paimon_cache.clone());

    let raw_meta = std::fs::read_to_string(&meta_path).unwrap();
    let rows: Vec<Value> = raw_meta
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(rows.len() >= 2000);

    // Map/underground transition indices recorded at paimon_pause frames where map_id/layer changes
    let transition_indices: HashSet<usize> = [307usize, 578, 737, 918, 1915].into_iter().collect();

    let mut total_active = 0usize;
    let mut tracking_active = 0usize;
    let mut pos_errs = Vec::new();
    let mut heading_errs = Vec::new();
    let mut trusted_heading_errs = Vec::new();
    let mut fixed_cone_checks: HashMap<usize, f64> = HashMap::new();
    let mut paimon_pauses = 0usize;
    let mut t_replay = Instant::now();
    let mut last_ts = 0.0f64;

    for row in &rows {
        let i = row["i"].as_u64().unwrap() as usize;
        let ts = row["ts"].as_f64().unwrap();
        let fw = row["frame_w"].as_u64().unwrap() as u32;
        let fh = row["frame_h"].as_u64().unwrap() as u32;
        let py_mid = row["map_id"].as_i64().unwrap() as i32;
        let py_wx = row["world_x"].as_f64().unwrap();
        let py_wy = row["world_y"].as_f64().unwrap();
        let py_ug = row["underground"].as_bool().unwrap();
        let py_match = row["match"].as_str().unwrap_or("");
        let py_cam = row["camera"].as_f64();
        let py_cam_src = row["cam_source"].as_str().unwrap_or("");

        if ts < last_ts {
            panic!("non-monotonic ts in meta.jsonl at i={i}");
        }
        last_ts = ts;

        if transition_indices.contains(&i) {
            tracker
                .set_map_m_anchor(
                    py_mid,
                    py_wx,
                    py_wy,
                    py_ug,
                    if py_ug { Some(1) } else { None },
                    if py_ug { "B1" } else { "G" },
                    Some(ts),
                )
                .unwrap();
            continue;
        }

        let frame_path = rec_dir.join("frames").join(format!("{i:06}.png"));
        let decoded = read_image(&frame_path, ImageReadMode::Unchanged).unwrap();
        let pose = match decoded {
            DecodedImage::Bgra(arr) => tracker
                .step_from_hud_array3(Some(arr.view()), fw, fh, Some(ts), false, None)
                .unwrap(),
            DecodedImage::Bgr(arr) => tracker
                .step_from_hud_array3(Some(arr.view()), fw, fh, Some(ts), false, None)
                .unwrap(),
            DecodedImage::Gray(arr) => {
                let bgr = DecodedImage::Gray(arr).into_bgr().unwrap();
                tracker
                    .step_from_hud_array3(Some(bgr.view()), fw, fh, Some(ts), false, None)
                    .unwrap()
            }
        };

        if pose.match_method == "paimon_pause" {
            paimon_pauses += 1;
        }

        if py_match != "paimon_pause" {
            total_active += 1;
            if pose.state == TrackerState::Tracking {
                tracking_active += 1;
            }
            if let (Some(c), Some(pc)) = (pose.camera_heading, py_cam) {
                let derr = shortest_angle_delta(pc, c).abs();
                heading_errs.push(derr);
                if py_cam_src == "bgr_blend" {
                    trusted_heading_errs.push(derr);
                }
            }
            if matches!(i, 86 | 121 | 408 | 659)
                && let Some(c) = pose.camera_heading
            {
                fixed_cone_checks.insert(i, c);
            }
            if py_mid == 2
                && !py_ug
                && matches!(
                    py_match,
                    "hd_sift" | "hd_sobel_fast" | "global_teyvat_relocate"
                )
                && let (Some(rx), Some(ry)) = (pose.world_x, pose.world_y)
            {
                pos_errs.push((rx - py_wx).hypot(ry - py_wy));
            }
        }
    }

    let elapsed_s = t_replay.elapsed().as_secs_f64();
    let tracking_pct = 100.0 * tracking_active as f64 / total_active.max(1) as f64;
    let med_pos = median_f64(&mut pos_errs);
    let p95_pos = percentile_f64(&mut pos_errs, 0.95);
    let med_hdg = median_f64(&mut heading_errs);
    let p95_hdg_all = percentile_f64(&mut heading_errs, 0.95);
    let p95_hdg_trusted = percentile_f64(&mut trusted_heading_errs, 0.95);
    let p99_hdg_trusted = percentile_f64(&mut trusted_heading_errs, 0.99);
    let _ = &mut t_replay;

    assert!(
        tracker.has_paimon_tpl() && paimon_cache.is_file(),
        "Paimon auto-bootstrap did not complete during 2191-frame replay"
    );
    assert!(paimon_pauses >= 100, "paimon_pauses={paimon_pauses}");
    assert!(
        (tracking_pct - 100.0).abs() < 1e-6,
        "expected 100% Tracking on active frames, got {tracking_pct:.2}% ({tracking_active}/{total_active})"
    );
    assert!(med_pos <= 6.0, "median position error vs Python too high: {med_pos:.2}w");
    assert!(p95_pos <= 25.0, "p95 position error vs Python too high: {p95_pos:.2}w");
    assert!(med_hdg < 2.0, "median heading delta too high: {med_hdg:.2} deg");
    // Enforce <10 deg p95 against all trusted Python `bgr_blend` frames (~1149 frames);
    // on the remaining ~254 `hsv_adaptive`/`body` frames the legacy Python oracle itself was
    // off by 15-135 deg while Joint White-Veil tracks the real camera cone (verified below).
    assert!(
        trusted_heading_errs.len() >= 1100,
        "expected >=1100 trusted python bgr_blend frames, got {}",
        trusted_heading_errs.len()
    );
    assert!(
        p95_hdg_trusted < 10.0,
        "p95 trusted heading delta too high: {p95_hdg_trusted:.2} deg (all={p95_hdg_all:.2} deg)"
    );
    for &(idx, gt_deg) in &[(86usize, 96.3f64), (121, 20.4), (408, 31.6), (659, 68.9)] {
        let got = fixed_cone_checks
            .get(&idx)
            .copied()
            .unwrap_or_else(|| panic!("missing camera_heading at visually verified frame {idx}"));
        let err = shortest_angle_delta(gt_deg, got).abs();
        assert!(
            err <= 10.0,
            "visually verified frame {idx} expected ~{gt_deg:.1} deg, got {got:.1} deg (err={err:.1})"
        );
    }

    eprintln!(
        "e2_rec_20261122_hud_replay_parity PASS (frames={}, active={}/{}, med_pos={med_pos:.2}w, p95_pos={p95_pos:.2}w, med_hdg={med_hdg:.3}deg, p95_trusted={p95_hdg_trusted:.3}deg, p99_trusted={p99_hdg_trusted:.3}deg, p95_all={p95_hdg_all:.3}deg, pauses={paimon_pauses}, elapsed={elapsed_s:.2}s)",
        rows.len(),
        tracking_active,
        total_active
    );
}

#[test]
#[ignore]
fn e2_yt_bench_curate_gate_and_headings() {
    let root = repo_root();
    let curate_dir = root.join("scratch").join("yt_bench").join("curate");
    let gt_path = curate_dir.join("gt.json");
    let live_rec_path = root
        .join("scratch")
        .join("yt_bench")
        .join("cache")
        .join("live_rec.npz");
    if !gt_path.is_file() || !live_rec_path.is_file() {
        eprintln!("SKIP e2_yt_bench_curate_gate_and_headings: curate/gt.json or live_rec.npz missing");
        return;
    }

    let gt_map: BTreeMap<String, f64> =
        serde_json::from_str(&std::fs::read_to_string(&gt_path).unwrap()).unwrap();
    assert!(!gt_map.is_empty());

    let mm_npy = NpzArchive::read_single_from_file(&live_rec_path, "mm").unwrap();
    let sh = mm_npy.shape();
    assert_eq!(sh.len(), 4);
    let (n_frames, mh, mw, mc) = (sh[0], sh[1], sh[2], sh[3]);
    let mm_data = mm_npy.as_u8().unwrap();
    let stride = mh * mw * mc;

    let mut gate = MinimapGate::new(12.0, 1.2, 0.22, true);
    let tmp = TempDirGuard::new("gh_e2_yt");
    let tc = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: 16,
            tile_size: 256,
            npz_path: None,
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: BTreeMap::new(),
            tile_provider: None,
            async_fetch: false,
        })
        .unwrap(),
    );
    let cfg = MinimapTrackerConfig::production_defaults();

    let mut gate_ok = 0usize;
    let mut heading_errs = Vec::new();
    for (id_str, gt_deg) in &gt_map {
        let idx: usize = id_str.parse().unwrap();
        if idx >= n_frames {
            continue;
        }
        let fr = ndarray::ArrayView3::from_shape(
            (mh, mw, mc),
            &mm_data[idx * stride..(idx + 1) * stride],
        )
        .unwrap();
        if gate.check_array3(Some(fr), false).valid {
            gate_ok += 1;
        }
        let mut tr = HybridMinimapTracker::new(Arc::clone(&tc), cfg.clone()).unwrap();
        tr.set_map_m_anchor(2, 0.0, 0.0, false, None, "G", Some(1.0))
            .unwrap();
        if let Some(p) = tr.step_headings_only_array3(Some(fr), Some(1.05))
            && let Some(cam) = p.camera_heading
        {
            heading_errs.push(shortest_angle_delta(*gt_deg, cam).abs());
        }
    }

    let mut contact_rejected = 0usize;
    for entry in std::fs::read_dir(&curate_dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("png") {
            let dec = read_image(&p, ImageReadMode::Unchanged).unwrap();
            let arr = match dec {
                DecodedImage::Bgra(a) => a,
                DecodedImage::Bgr(a) => a,
                DecodedImage::Gray(a) => DecodedImage::Gray(a).into_bgr().unwrap(),
            };
            let res = gate.check_array3(Some(arr.view()), false);
            if !res.valid && res.reason.as_deref() == Some("invalid_dimensions") {
                contact_rejected += 1;
            }
        }
    }
    assert!(contact_rejected >= 10, "expected >= 10 2:1 contact sheets rejected by MinimapGate, got {contact_rejected}");
    assert!(gate_ok >= 35, "expected >= 35 gate_ok in live_rec gt, got {gate_ok}/{}", gt_map.len());

    // Negative flat check
    let flat = Array3::<u8>::from_elem((218, 218, 3), 80);
    assert!(!gate.check_array3(Some(flat.view()), false).valid);

    let med_err = median_f64(&mut heading_errs);
    assert!(med_err < 2.0, "yt_bench median heading error={med_err:.2}");
    eprintln!(
        "e2_yt_bench_curate_gate_and_headings PASS (gate_ok={gate_ok}/{}, contact_rejected={contact_rejected}, med_err={med_err:.3}deg)",
        gt_map.len()
    );
}

#[test]
#[ignore]
fn e2_slider_probe_1440p_latency_and_accuracy() {
    let root = repo_root();
    let game_fp = root
        .join("scratch")
        .join("diagnostics")
        .join("diag_20261122_161651")
        .join("01_game.png");
    let z0_fp = root.join("scratch").join("0zoom.png");
    let z100_fp = root.join("scratch").join("100zoom.png");
    if !game_fp.is_file() || !z0_fp.is_file() || !z100_fp.is_file() {
        eprintln!("SKIP e2_slider_probe_1440p_latency_and_accuracy: scratch fixtures missing");
        return;
    }

    let game = read_image_bgr(&game_fp).expect("read 01_game.png");
    let (gh, gw, _) = game.dim();
    let grect = MapScreenStateMachine::get_slider_probe_rect(0, 0, gw as i32, gh as i32);
    let groi = game.slice(s![
        grect.y as usize..(grect.y + grect.height) as usize,
        grect.x as usize..(grect.x + grect.width) as usize,
        ..
    ]);

    // Warm up FFT plan cache & template cache
    let (g_open, g_score) = MapScreenStateMachine::is_slider_probe(Some(groi), gw, gh);
    assert!(!g_open, "01_game.png false positive slider probe score={g_score}");

    let mut lats = Vec::with_capacity(20);
    for _ in 0..20 {
        let t0 = Instant::now();
        let (open, _) = MapScreenStateMachine::is_slider_probe(Some(groi), gw, gh);
        lats.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(!open);
    }
    let med_ms = median_f64(&mut lats);

    for fp in [&z0_fp, &z100_fp] {
        let img = read_image_bgr(fp).expect("read zoom png");
        let (ih, iw, _) = img.dim();
        let r = MapScreenStateMachine::get_slider_probe_rect(0, 0, iw as i32, ih as i32);
        let roi = img.slice(s![
            r.y as usize..(r.y + r.height) as usize,
            r.x as usize..(r.x + r.width) as usize,
            ..
        ]);
        let (open, score) = MapScreenStateMachine::is_slider_probe(Some(roi), iw, ih);
        assert!(open, "missed map slider on {:?} score={score}", fp.file_name());
    }

    eprintln!(
        "e2_slider_probe_1440p_latency_and_accuracy ({}x{} -> roi {}x{}) med={med_ms:.2}ms",
        gw, gh, grect.width, grect.height
    );
    assert!(
        med_ms <= 3.5,
        "1440p slider probe median latency {med_ms:.2}ms > 3.5ms"
    );
}
