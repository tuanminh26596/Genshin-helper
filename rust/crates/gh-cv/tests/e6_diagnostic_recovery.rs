//! Track E6 Offline Diagnostic Recovery Regression Suite.
//!
//! - Fast always-run unit test validates structured `DiagnosticRecoveryReport` metrics calculation.
//! - `#[ignore]` fixture suite (`e6_diagnostic_bundles_recovery_regression`) replays real
//!   `scratch/diagnostics/diag_*` bundles (`01_game.png` + `03_report.json` + optional
//!   `07_pre_lost_frames/` minimap sequence) against `MapLocator` and `HybridMinimapTracker`
//!   and enforces zero false-lock regressions.
//!
//! Run full offline suite with:
//! `cargo nextest run --manifest-path rust/Cargo.toml -p gh-cv --test e6_diagnostic_recovery --profile cv-heavy --cargo-profile cv-bench --run-ignored all`

use gh_core::CoreResult;
use gh_core::frame::FrameView;
use gh_core::multimap::load_multimap_db;
use gh_core::npz::{NpyData, NpzArchive};
use gh_core::traits::OcrProvider;
use gh_core::ui_layout::compute_ui_layout;
use gh_cv::global_relocator::TeyvatGlobalRelocator;
use gh_cv::image_io::read_image_bgr;
use gh_cv::locator::{LocatorPaths, MapLocator, TerrainRaster};
use gh_cv::minimap_tracker::{HybridMinimapTracker, MinimapTrackerConfig, TrackerState};
use gh_cv::tile_cache::{LRU_TILE_CAPACITY, LruTerrainTileCache, TileCacheConfig};
use ndarray::{Array2, s};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleStatus {
    Recovered,
    TransientRecovered,
    Unresolved,
    FalseLock,
}

impl BundleStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Recovered => "RECOVERED",
            Self::TransientRecovered => "TRANSIENT_RECOVERED",
            Self::Unresolved => "UNRESOLVED",
            Self::FalseLock => "FALSE_LOCK",
        }
    }
}

#[derive(Debug, Clone)]
struct BundleRecoveryOutcome {
    bundle: String,
    map_id: Option<i32>,
    status: BundleStatus,
    pos_err_w: Option<f64>,
    recovery_ms: f64,
    false_lock: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct DiagnosticRecoveryReport {
    total_bundles: usize,
    recovered: usize,
    transient_recovered: usize,
    unresolved: usize,
    false_lock_count: usize,
    recovery_time_ms: f64,
    median_pos_err_w: Option<f64>,
}

fn compute_median(vals: &[f64]) -> Option<f64> {
    let mut clean: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    if clean.is_empty() {
        return None;
    }
    clean.sort_by(f64::total_cmp);
    let n = clean.len();
    if n % 2 == 1 {
        Some(clean[n / 2])
    } else {
        Some(0.5 * (clean[n / 2 - 1] + clean[n / 2]))
    }
}

impl DiagnosticRecoveryReport {
    fn from_outcomes(outcomes: &[BundleRecoveryOutcome]) -> Self {
        let mut recovered = 0usize;
        let mut transient_recovered = 0usize;
        let mut unresolved = 0usize;
        let mut false_lock_count = 0usize;
        let mut pos_errs = Vec::new();
        let mut recovered_times = Vec::new();
        let mut all_times = Vec::new();

        for o in outcomes {
            if o.false_lock || o.status == BundleStatus::FalseLock {
                false_lock_count += 1;
            }
            match o.status {
                BundleStatus::Recovered => {
                    recovered += 1;
                    if o.recovery_ms.is_finite() {
                        recovered_times.push(o.recovery_ms);
                    }
                }
                BundleStatus::TransientRecovered => {
                    transient_recovered += 1;
                    if o.recovery_ms.is_finite() {
                        recovered_times.push(o.recovery_ms);
                    }
                }
                BundleStatus::Unresolved => {
                    unresolved += 1;
                }
                BundleStatus::FalseLock => {}
            }
            if o.recovery_ms.is_finite() {
                all_times.push(o.recovery_ms);
            }
            if let Some(err) = o.pos_err_w
                && err.is_finite()
            {
                pos_errs.push(err);
            }
        }

        let recovery_time_ms = compute_median(&recovered_times)
            .or_else(|| compute_median(&all_times))
            .unwrap_or(0.0);
        let median_pos_err_w = compute_median(&pos_errs);

        Self {
            total_bundles: outcomes.len(),
            recovered,
            transient_recovered,
            unresolved,
            false_lock_count,
            recovery_time_ms,
            median_pos_err_w,
        }
    }
}

fn format_metric_table(
    outcomes: &[BundleRecoveryOutcome],
    report: &DiagnosticRecoveryReport,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<24} | {:>6} | {:<19} | {:>9} | {:>11} | {:<10}",
        "bundle", "map_id", "status", "pos_err_w", "recovery_ms", "false_lock"
    );
    let _ = writeln!(out, "{}", "-".repeat(94));
    for o in outcomes {
        let mid_s = o
            .map_id
            .map_or_else(|| "-".to_owned(), |m| m.to_string());
        let err_s = o
            .pos_err_w
            .filter(|v| v.is_finite())
            .map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"));
        let _ = writeln!(
            out,
            "{:<24} | {:>6} | {:<19} | {:>9} | {:>11.2} | {:<10}",
            o.bundle,
            mid_s,
            o.status.as_str(),
            err_s,
            o.recovery_ms,
            o.false_lock
        );
    }
    let _ = writeln!(out, "{}", "-".repeat(94));
    let med_err = report
        .median_pos_err_w
        .map_or_else(|| "-".to_owned(), |v| format!("{v:.2}w"));
    let _ = writeln!(
        out,
        "SUMMARY: total={} recovered={} transient_recovered={} unresolved={} \
         false_lock_count={} recovery_time_ms={:.2} median_pos_err_w={}",
        report.total_bundles,
        report.recovered,
        report.transient_recovered,
        report.unresolved,
        report.false_lock_count,
        report.recovery_time_ms,
        med_err
    );
    out
}

struct ReportOcrProvider {
    current: Mutex<String>,
}

impl ReportOcrProvider {
    fn new() -> Self {
        Self {
            current: Mutex::new(String::new()),
        }
    }

    fn set(&self, text: &str) {
        if let Ok(mut g) = self.current.lock() {
            *g = text.to_owned();
        }
    }
}

impl OcrProvider for ReportOcrProvider {
    fn recognize_text(&self, _frame: FrameView<'_>) -> CoreResult<String> {
        Ok(self
            .current
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct GroundTruthOverride {
    expect_rejected: bool,
    expected_map_id: Option<i32>,
    ref_xy: Option<(f64, f64)>,
}

/// Loads human-verified ground truth overrides from tracked `tests/verified_gt.json`.
/// Keeping verified GT in a git-tracked JSON file ensures any accidental modification shows up
/// immediately in `git diff` and lets `gh-sim import-diag` add new cases without editing Rust code.
fn load_verified_gt_overrides(root: &Path) -> BTreeMap<String, GroundTruthOverride> {
    let path = root.join("tests").join("verified_gt.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("failed to read tracked ground truth file {}: {e}", path.display())
    });
    let doc: Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("invalid JSON in {}: {e}", path.display())
    });
    let mut out = BTreeMap::new();
    if let Some(diags) = doc.get("diagnostics").and_then(Value::as_object) {
        for (bundle_name, entry) in diags {
            let expect_rejected = entry
                .get("expect_rejected")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let expected_map_id = entry
                .get("expected_map_id")
                .and_then(Value::as_i64)
                .map(|v| v as i32);
            let ref_xy = entry.get("ref_xy").and_then(Value::as_array).and_then(|arr| {
                if arr.len() == 2 {
                    let x = arr[0].as_f64()?;
                    let y = arr[1].as_f64()?;
                    if x.is_finite() && y.is_finite() {
                        return Some((x, y));
                    }
                }
                None
            });
            out.insert(
                bundle_name.clone(),
                GroundTruthOverride {
                    expect_rejected,
                    expected_map_id,
                    ref_xy,
                },
            );
        }
    }
    out
}

fn is_strong_minimap_match(method: &str) -> bool {
    matches!(
        method,
        "hd_sift"
            | "hd_sift_reacquire"
            | "hd_sift_tier2"
            | "hd_sobel"
            | "hd_sobel_fast"
            | "global_teyvat_relocate"
            | "annulus_template"
            | "annulus_template_zoom_switch"
            | "follow_view_annulus"
            | "follow_view_orb"
            | "layer_switch_sift"
    )
}

fn collect_pngs_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("png"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn extract_nav_player_anchor(report_json: &Value) -> Option<(i32, f64, f64, bool, Option<i32>)> {
    let player = report_json.get("nav_state")?.get("player")?;
    let map_id = player.get("map_id")?.as_i64()? as i32;
    // HoYoLAB Leaflet convention: lng == world_x, lat == world_y.
    let wx = player.get("lng")?.as_f64()?;
    let wy = player.get("lat")?.as_f64()?;
    if !wx.is_finite() || !wy.is_finite() {
        return None;
    }
    let is_ug = player
        .get("is_underground")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let floor_id = player
        .get("floor_id")
        .and_then(|v| {
            v.as_i64()
                .map(|n| n as i32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<i32>().ok()))
        });
    Some((map_id, wx, wy, is_ug, floor_id))
}

#[allow(clippy::too_many_arguments)]
fn replay_pre_lost_sequence(
    bundle_name: &str,
    frames: &[PathBuf],
    report_json: &Value,
    tile_cache: Arc<LruTerrainTileCache>,
    db: Arc<gh_core::multimap::MultimapDb>,
    relocator: Option<Arc<TeyvatGlobalRelocator>>,
) -> BundleRecoveryOutcome {
    let mut tracker = HybridMinimapTracker::new(tile_cache, MinimapTrackerConfig::production_defaults())
        .expect("init HybridMinimapTracker")
        .with_multimap_db(db);
    if let Some(reloc) = relocator {
        tracker = tracker.with_global_relocator(reloc);
    }

    let anchor_info = extract_nav_player_anchor(report_json).or_else(|| {
        let cv = report_json.get("cv_telemetry")?;
        if !cv.get("has_world_coords").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }
        let mid = cv.get("map_id")?.as_i64()? as i32;
        let wx = cv.get("world_x")?.as_f64()?;
        let wy = cv.get("world_y")?.as_f64()?;
        let is_ug = cv.get("is_underground").and_then(Value::as_bool).unwrap_or(false);
        let fid = cv.get("floor_id").and_then(Value::as_i64).map(|n| n as i32);
        Some((mid, wx, wy, is_ug, fid))
    });

    let mut t = 0.0f64;
    if let Some((mid, wx, wy, is_ug, fid)) = anchor_info {
        let fname = if is_ug { "B1" } else { "G" };
        let _ = tracker.set_map_m_anchor(mid, wx, wy, is_ug, fid, fname, Some(t));
    }

    let t0 = Instant::now();
    let mut saw_degraded = false;
    let mut recovered_after_degraded = false;
    let mut last_pose = None;
    for fp in frames {
        let Ok(img) = read_image_bgr(fp) else {
            continue;
        };
        t += 0.1;
        let (h, w, _) = img.dim();
        let aspect = w as f64 / h.max(1) as f64;
        let pose = if h.min(w) <= 420 && (0.75..=1.33).contains(&aspect) {
            tracker.step_array3(Some(img.view()), Some(t), None).ok()
        } else {
            tracker
                .step_from_hud_array3(Some(img.view()), w as u32, h as u32, Some(t), false, None)
                .ok()
        };
        if let Some(p) = pose {
            let strong = p.state == TrackerState::Tracking && is_strong_minimap_match(&p.match_method);
            if !strong {
                saw_degraded = true;
            } else if saw_degraded {
                recovered_after_degraded = true;
            }
            last_pose = Some(p);
        }
    }
    let recovery_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let Some(final_pose) = last_pose else {
        return BundleRecoveryOutcome {
            bundle: bundle_name.to_owned(),
            map_id: anchor_info.map(|a| a.0),
            status: BundleStatus::Unresolved,
            pos_err_w: None,
            recovery_ms,
            false_lock: false,
        };
    };

    let pos_err_w = match (anchor_info, final_pose.world_x, final_pose.world_y) {
        (Some((_, ax, ay, _, _)), Some(fx), Some(fy)) if fx.is_finite() && fy.is_finite() => {
            Some((fx - ax).hypot(fy - ay))
        }
        _ => None,
    };
    let map_mismatch = anchor_info.is_some_and(|(mid, _, _, _, _)| {
        final_pose.map_id.is_some_and(|fmid| fmid != mid)
    });
    let false_lock = map_mismatch || pos_err_w.is_some_and(|e| e > 250.0);
    let status = if false_lock {
        BundleStatus::FalseLock
    } else if final_pose.state == TrackerState::Tracking && final_pose.world_x.is_some() {
        if recovered_after_degraded {
            BundleStatus::TransientRecovered
        } else {
            BundleStatus::Recovered
        }
    } else {
        BundleStatus::Unresolved
    };

    BundleRecoveryOutcome {
        bundle: bundle_name.to_owned(),
        map_id: final_pose.map_id.or_else(|| anchor_info.map(|a| a.0)),
        status,
        pos_err_w,
        recovery_ms,
        false_lock,
    }
}

#[test]
fn e6_diagnostic_recovery_report_metrics_calculation() {
    let empty = DiagnosticRecoveryReport::from_outcomes(&[]);
    assert_eq!(empty.total_bundles, 0);
    assert_eq!(empty.recovered, 0);
    assert_eq!(empty.transient_recovered, 0);
    assert_eq!(empty.unresolved, 0);
    assert_eq!(empty.false_lock_count, 0);
    assert!((empty.recovery_time_ms - 0.0).abs() < 1e-12);
    assert_eq!(empty.median_pos_err_w, None);

    let outcomes = vec![
        BundleRecoveryOutcome {
            bundle: "diag_a".into(),
            map_id: Some(2),
            status: BundleStatus::Recovered,
            pos_err_w: Some(1.5),
            recovery_ms: 12.0,
            false_lock: false,
        },
        BundleRecoveryOutcome {
            bundle: "diag_b".into(),
            map_id: Some(2),
            status: BundleStatus::TransientRecovered,
            pos_err_w: Some(4.5),
            recovery_ms: 30.0,
            false_lock: false,
        },
        BundleRecoveryOutcome {
            bundle: "diag_c".into(),
            map_id: Some(36),
            status: BundleStatus::TransientRecovered,
            pos_err_w: Some(2.5),
            recovery_ms: 18.0,
            false_lock: false,
        },
        BundleRecoveryOutcome {
            bundle: "diag_d".into(),
            map_id: None,
            status: BundleStatus::Unresolved,
            pos_err_w: Some(f64::NAN),
            recovery_ms: 95.0,
            false_lock: false,
        },
        BundleRecoveryOutcome {
            bundle: "diag_e".into(),
            map_id: Some(40),
            status: BundleStatus::FalseLock,
            pos_err_w: Some(420.0),
            recovery_ms: 25.0,
            false_lock: true,
        },
    ];

    let report = DiagnosticRecoveryReport::from_outcomes(&outcomes);
    assert_eq!(report.total_bundles, 5);
    assert_eq!(report.recovered, 1);
    assert_eq!(report.transient_recovered, 2);
    assert_eq!(report.unresolved, 1);
    assert_eq!(report.false_lock_count, 1);
    // Median of recovered/transient recovery_ms [12.0, 18.0, 30.0] == 18.0.
    assert!((report.recovery_time_ms - 18.0).abs() < 1e-9);
    // Median of finite pos_err_w [1.5, 2.5, 4.5, 420.0] == (2.5 + 4.5) / 2 = 3.5.
    assert!((report.median_pos_err_w.unwrap() - 3.5).abs() < 1e-9);

    let table = format_metric_table(&outcomes, &report);
    for col in [
        "bundle",
        "map_id",
        "status",
        "pos_err_w",
        "recovery_ms",
        "false_lock",
        "diag_b",
        "TRANSIENT_RECOVERED",
    ] {
        assert!(table.contains(col), "metric table missing '{col}':\n{table}");
    }
}

#[test]
fn e6_verified_gt_json_loads_historical_entries() {
    let gt = load_verified_gt_overrides(&repo_root());
    assert!(
        gt.len() >= 9,
        "expected migrated historical GT overrides in tests/verified_gt.json, got {}",
        gt.len()
    );
    assert!(gt["diag_20261121_011520"].expect_rejected);
    assert_eq!(gt["diag_20260920_214102"].expected_map_id, Some(36));
    assert_eq!(gt["diag_20261120_231651"].ref_xy, Some((-5541.9, -7014.9)));
}

#[test]
#[ignore]
fn e6_diagnostic_bundles_recovery_regression() {
    let root = repo_root();
    let verified_gt = load_verified_gt_overrides(&root);
    let diag_dir = root.join("scratch").join("diagnostics");
    let db_path = multimap_json_path(&root);
    let terrain_path = terrain_npz_path(&root);
    if !diag_dir.is_dir() || !db_path.is_file() || !terrain_path.is_file() {
        eprintln!(
            "SKIP e6_diagnostic_bundles_recovery_regression: scratch/diagnostics/ or local data/ absent"
        );
        return;
    }

    let mut bundle_dirs: Vec<PathBuf> = std::fs::read_dir(&diag_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("diag_"))
        })
        .collect();
    bundle_dirs.sort();
    if bundle_dirs.is_empty() {
        eprintln!("SKIP e6_diagnostic_bundles_recovery_regression: no diag_* bundles found");
        return;
    }

    let ocr = Arc::new(ReportOcrProvider::new());
    let locator = MapLocator::with_paths(
        LocatorPaths {
            db_path: Some(db_path.clone()),
            terrain_npz_path: Some(terrain_path.clone()),
            cave_masks_npz_path: Some(root.join("data").join("cave_masks_v4.npz")),
            ui_anchors_npz_path: Some(root.join("data").join("ui_anchors_v4.npz")),
            legacy_underground_db_path: Some(root.join("data").join("underground_db.json")),
        },
        Some(ocr.clone()),
    );

    let db = Arc::new(load_multimap_db(&db_path).expect("load multimap_v4.json"));
    let tmp = TempDirGuard::new("gh_e6_diag_recovery");
    let tile_cache = Arc::new(
        LruTerrainTileCache::from_config(TileCacheConfig {
            max_tiles: LRU_TILE_CAPACITY,
            tile_size: 256,
            npz_path: Some(terrain_path.clone()),
            use_default_npz: false,
            disk_cache_dir: Some(tmp.0.join("tiles")),
            terrain_data: locator.terrain_data().clone(),
            tile_provider: None,
            async_fetch: false,
        })
        .expect("init LruTerrainTileCache"),
    );

    let mut lazy_relocator: Option<Arc<TeyvatGlobalRelocator>> = None;
    let mut outcomes = Vec::with_capacity(bundle_dirs.len());

    for bundle_dir in &bundle_dirs {
        let bundle_name = bundle_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_owned();
        let report_path = bundle_dir.join("03_report.json");
        let report_json: Value = std::fs::read_to_string(&report_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);

        let pre_lost_dir = bundle_dir.join("07_pre_lost_frames");
        if pre_lost_dir.is_dir() {
            let pre_frames = collect_pngs_sorted(&pre_lost_dir);
            if !pre_frames.is_empty() {
                if lazy_relocator.is_none()
                    && let Some(r2) = load_map2_raster(&root)
                {
                    let idx_path = tmp.0.join("teyvat_global_index_v3.npz");
                    let reloc = Arc::new(TeyvatGlobalRelocator::new(Some(r2), Some(idx_path), false));
                    if reloc.is_ready() {
                        lazy_relocator = Some(reloc);
                    }
                }
                outcomes.push(replay_pre_lost_sequence(
                    &bundle_name,
                    &pre_frames,
                    &report_json,
                    Arc::clone(&tile_cache),
                    Arc::clone(&db),
                    lazy_relocator.clone(),
                ));
                continue;
            }
        }

        let game_png = bundle_dir.join("01_game.png");
        let Ok(img) = read_image_bgr(&game_png) else {
            outcomes.push(BundleRecoveryOutcome {
                bundle: bundle_name,
                map_id: None,
                status: BundleStatus::Unresolved,
                pos_err_w: None,
                recovery_ms: 0.0,
                false_lock: false,
            });
            continue;
        };

        let cv_tel = report_json.get("cv_telemetry");
        let region_text = cv_tel
            .and_then(|c| c.get("region_text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let report_detected = cv_tel
            .and_then(|c| c.get("detected"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let override_gt = verified_gt.get(&bundle_name);
        let expect_rejected = override_gt.map_or(!report_detected, |gt| gt.expect_rejected);

        locator.reset_tracking_state();
        ocr.set(region_text);

        let t0 = Instant::now();
        let loc_res = locator.locate_player(img.view(), true, None);
        let loc_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if expect_rejected {
            // Non-M-map HUD screenshot or off-screen edge compass: MapLocator must NOT false-lock.
            if let Some(res) = loc_res
                && res.has_world_coords
            {
                outcomes.push(BundleRecoveryOutcome {
                    bundle: bundle_name,
                    map_id: res.map_id,
                    status: BundleStatus::FalseLock,
                    pos_err_w: None,
                    recovery_ms: loc_ms,
                    false_lock: true,
                });
                continue;
            }

            // For open-world HUD bundles with a surface nav_state anchor, probe transient
            // HybridMinimapTracker recovery from a perturbed anchor on the top-left HUD crop.
            if !report_detected
                && let Some((nav_mid, ref_wx, ref_wy, false, None)) =
                    extract_nav_player_anchor(&report_json)
                && locator.has_terrain_for_map(nav_mid)
            {
                let (fh, fw, _) = img.dim();
                if let Ok(lay) = compute_ui_layout(fw as u32, fh as u32, false) {
                    let hud_w = ((lay.minimap_rect.x + lay.minimap_rect.width + 64) as usize)
                        .min(fw);
                    let hud_h = ((lay.minimap_rect.y + lay.minimap_rect.height + 64) as usize)
                        .min(fh);
                    let mut tracker = HybridMinimapTracker::new(
                        Arc::clone(&tile_cache),
                        MinimapTrackerConfig::production_defaults(),
                    )
                    .expect("init HUD tracker")
                    .with_multimap_db(Arc::clone(&db));
                    let _ = tracker.set_map_m_anchor(
                        nav_mid,
                        ref_wx + 12.0,
                        ref_wy - 8.0,
                        false,
                        None,
                        "G",
                        Some(1.0),
                    );
                    let t_mm = Instant::now();
                    let hud_crop = img.slice(s![0..hud_h, 0..hud_w, ..]);
                    if let Ok(pose) = tracker.step_from_hud_array3(
                        Some(hud_crop),
                        fw as u32,
                        fh as u32,
                        Some(1.1),
                        false,
                        None,
                    ) {
                        let mm_ms = t_mm.elapsed().as_secs_f64() * 1000.0;
                        if pose.state == TrackerState::Tracking
                            && is_strong_minimap_match(&pose.match_method)
                            && let (Some(px), Some(py)) = (pose.world_x, pose.world_y)
                        {
                            let err = (px - ref_wx).hypot(py - ref_wy);
                            let false_lock = !err.is_finite() || err > 120.0;
                            outcomes.push(BundleRecoveryOutcome {
                                bundle: bundle_name,
                                map_id: Some(nav_mid),
                                status: if false_lock {
                                    BundleStatus::FalseLock
                                } else {
                                    BundleStatus::TransientRecovered
                                },
                                pos_err_w: Some(err),
                                recovery_ms: mm_ms,
                                false_lock,
                            });
                            continue;
                        }
                    }
                }
            }

            outcomes.push(BundleRecoveryOutcome {
                bundle: bundle_name,
                map_id: extract_nav_player_anchor(&report_json).map(|a| a.0),
                status: BundleStatus::Recovered,
                pos_err_w: None,
                recovery_ms: loc_ms,
                false_lock: false,
            });
            continue;
        }

        let expected_map_id = override_gt
            .as_ref()
            .and_then(|gt| gt.expected_map_id)
            .or_else(|| {
                cv_tel
                    .and_then(|c| c.get("map_id"))
                    .and_then(Value::as_i64)
                    .map(|m| m as i32)
            });
        let ref_xy = match &override_gt {
            Some(gt) => gt.ref_xy,
            None => cv_tel.and_then(|c| {
                let wx = c.get("world_x")?.as_f64()?;
                let wy = c.get("world_y")?.as_f64()?;
                if wx.is_finite() && wy.is_finite() {
                    Some((wx, wy))
                } else {
                    None
                }
            }),
        };

        match loc_res {
            Some(res) if res.has_world_coords => {
                let (Some(wx), Some(wy)) = (res.world_x, res.world_y) else {
                    outcomes.push(BundleRecoveryOutcome {
                        bundle: bundle_name,
                        map_id: res.map_id,
                        status: BundleStatus::Unresolved,
                        pos_err_w: None,
                        recovery_ms: loc_ms,
                        false_lock: false,
                    });
                    continue;
                };
                let pos_err_w = ref_xy.map(|(rx, ry)| (wx - rx).hypot(wy - ry));
                let map_mismatch = expected_map_id.is_some_and(|exp| res.map_id != Some(exp));
                let false_lock = !wx.is_finite()
                    || !wy.is_finite()
                    || map_mismatch
                    || pos_err_w.is_some_and(|e| e > 120.0);
                outcomes.push(BundleRecoveryOutcome {
                    bundle: bundle_name,
                    map_id: res.map_id,
                    status: if false_lock {
                        BundleStatus::FalseLock
                    } else {
                        BundleStatus::Recovered
                    },
                    pos_err_w,
                    recovery_ms: loc_ms,
                    false_lock,
                });
            }
            _ => {
                outcomes.push(BundleRecoveryOutcome {
                    bundle: bundle_name,
                    map_id: expected_map_id,
                    status: BundleStatus::Unresolved,
                    pos_err_w: None,
                    recovery_ms: loc_ms,
                    false_lock: false,
                });
            }
        }
    }

    let report = DiagnosticRecoveryReport::from_outcomes(&outcomes);
    let table = format_metric_table(&outcomes, &report);
    eprintln!("\n{table}");

    assert_eq!(
        report.false_lock_count, 0,
        "Zero false-lock regressions permitted:\n{table}"
    );
    assert!(
        report.recovered + report.transient_recovered > 0,
        "Expected at least one recovered diagnostic bundle:\n{table}"
    );
    assert_eq!(
        report.unresolved, 0,
        "Expected all diagnostic bundles to resolve cleanly:\n{table}"
    );
}
