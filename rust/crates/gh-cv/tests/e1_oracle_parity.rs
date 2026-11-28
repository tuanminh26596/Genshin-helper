//! Track E1 `#[ignore]` Oracle Parity Tests on real `scratch/` fixtures.
//! Run with:
//! `cargo nextest run --manifest-path rust/Cargo.toml -p gh-cv --profile cv-heavy --cargo-profile cv-bench --run-ignored all`

use gh_core::CoreResult;
use gh_core::frame::{FrameBuf, FrameView, PixelFormat};
use gh_core::npz::NpzArchive;
use gh_core::traits::OcrProvider;
use gh_cv::heading360::{
    ConeMethod, extract_camera_view_cone, extract_player_arrow_360, shortest_angle_delta,
};
use gh_cv::image_io::read_image_bgr;
use gh_cv::locator::{
    LocateResult, LocatorPaths, MapLocator, WORLD_ANCHOR_MAP_IDS, WORLD_ANCHOR_Y_OFFSET,
};
use gh_cv::map_state::{
    ANCHOR_THRESHOLD, CLOSE_PROBE_DEFAULT_THRESHOLD, MapScreenStateMachine, MapState,
    SCENE_CUT_THRESHOLD,
};
use gh_cv::ops::Interpolation;
use gh_cv::ops::color::bgr_to_gray;
use gh_cv::ops::filters::gaussian_blur_u8;
use gh_cv::ops::resize::resize_u8_hwc;
use gh_cv::zoom_slider::ZoomSliderDetector;
use ndarray::{Array3, ArrayView3, s};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

fn collect_diag_game_files(root: &Path) -> Vec<PathBuf> {
    let diag_dir = root.join("scratch").join("diagnostics");
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&diag_dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_dir()
                && ent
                    .file_name()
                    .to_string_lossy()
                    .starts_with("diag_")
            {
                let game = p.join("01_game.png");
                if game.is_file() {
                    files.push(game);
                }
            }
        }
    }
    files.sort();
    files
}

fn collect_diag_browser_files(root: &Path) -> Vec<PathBuf> {
    let diag_dir = root.join("scratch").join("diagnostics");
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&diag_dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_dir()
                && ent
                    .file_name()
                    .to_string_lossy()
                    .starts_with("diag_")
            {
                let br = p.join("02_browser.png");
                if br.is_file() {
                    files.push(br);
                }
            }
        }
    }
    files.sort();
    files
}

fn collect_test_image_files(root: &Path) -> Vec<PathBuf> {
    let ti_dir = root.join("scratch").join("Test-images");
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ti_dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("png"))
            {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

fn diag_name(fp: &Path) -> String {
    fp.parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn bgr_to_bgra_frame_buf(bgr: ArrayView3<'_, u8>) -> FrameBuf {
    let (h, w, _) = bgr.dim();
    let mut fb = FrameBuf::new_zeroed(w as u32, h as u32, PixelFormat::Bgra8);
    for y in 0..h {
        for x in 0..w {
            let off = y * fb.stride + x * 4;
            fb.data[off] = bgr[[y, x, 0]];
            fb.data[off + 1] = bgr[[y, x, 1]];
            fb.data[off + 2] = bgr[[y, x, 2]];
            fb.data[off + 3] = 255;
        }
    }
    fb
}

struct FixtureOcrProvider {
    current: Mutex<String>,
}

impl FixtureOcrProvider {
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

impl OcrProvider for FixtureOcrProvider {
    fn recognize_text(&self, _frame: FrameView<'_>) -> CoreResult<String> {
        Ok(self
            .current
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default())
    }
}

fn ocr_fixture_text(key: &str) -> &'static str {
    match key {
        "diag_20260920_214102" => "Nüi Thånh Vién",
        "diag_20260920_214145" => "Nüi Thånh Vién cb",
        "diag_20260920_214359" => "I Dei Sånh Fontame",
        "diag_20260920_214528" => "Khu Beryl",
        "diag_20260920_214615" => "Khu Morte",
        "diag_20260920_214801" => "Lüng Tequemecan 0101",
        "diag_20260920_214910" => "Dåo Lempo",
        "diag_20260920_220106" => "Dei snh Fontaine",
        "diag_20260920_220225" => "Bech Du'0'ng Tuyét",
        "diag_20260920_220317" => "Lüng Löng Vü Lü'a",
        "diag_20260920_220416" => "Cao Nguyen Träng",
        "diag_20260920_220447" => "Cao Nguyen Träng",
        "diag_20260920_220624" => "Dåo Narukami",
        "diag_20260920_220745" => "CÖc - Thu'Q'ng C6c",
        "diag_20260920_221245" => "Lüng Tequemecan",
        "diag_20260920_222052" => "Lüng Tequemecan",
        "diag_20261120_204445" => "Ong Chåo Ven HÖa",
        "diag_20261120_204655" => "I Dai ånh Fontaine",
        "diag_20261120_205011" => "Novokitezhgrad",
        "diag_20261120_213109" => "I Da ånh Fontaine",
        "diag_20261120_231651" => "Novokitezhgrad",
        "diag_20261120_232016" => "Dinh Bäng Giå",
        "diag_20261120_232232" => "Lülhköng Vü Lü'a",
        "diag_20261120_232623" => "Riyng Lokapala",
        "diag_20261121_011215" => "Riyng Lokapala",
        "diag_20261121_011322" => "I Dai Sånh Fontaine",
        "diag_20261121_011520" => "Dich Chuyén",
        "diag_20261121_204617" => "Dinh Såi Cånh",
        "diag_20261123_003509" => "Tién ;öng Chåo Yen HÖa",
        "Screenshot 2026-09-19 232904.png" => "Dåo Lempe",
        "Screenshot 2026-09-19 234037.png" => "Ong Chåo Ven HÖa",
        "Screenshot 2026-09-19 234051.png" => "Löng Chåo Ven HÖa",
        "Screenshot 2026-09-20 004819.png" => "Chåo Van Höa",
        "Screenshot 2026-09-20 024639.png" => "Ngoqi Ö Vissudha",
        "Screenshot 2026-09-20 102526.png" => "Mac Hadramaveth",
        "Screenshot 2026-09-20 102641.png" => "Dåo Watatsumi",
        "Screenshot 2026-09-20 102719.png" => "Cao Nguyen Träng",
        "Screenshot 2026-09-20 102751.png" => "Cao Nguyén Träng",
        "Screenshot 2026-09-20 102847.png" => "Bién KY Nguyen Cü",
        "Screenshot 2026-09-20 102938.png" => "Bech Du'0'ng Tuyét",
        "Screenshot 2026-09-20 103023.png" => "Bech Du'0'ng Tuyét",
        "Screenshot 2026-09-20 103056.png" => "Dåo Lempo",
        "Screenshot 2026-09-20 103143.png" => "Dåo Hiisi",
        "Screenshot 2026-09-20 103226.png" => "Nüi Thånh Vién cb",
        "Screenshot 2026-09-20 103325.png" => "au - Khu MÖ Ngam",
        "Screenshot 2026-09-20 103356.png" => "au - Khu MÖ Ngam",
        "Screenshot 2026-09-20 103817.png" => "I Dai Sånh Fon me",
        "Screenshot 2026-09-20 103843.png" => "I Dai Sånh Fontaine",
        "Screenshot 2026-09-20 103958.png" => "I Dai Sånh Fontaine",
        "Screenshot 2026-09-20 104045.png" => "Khu Morte",
        "Screenshot 2026-09-20 104129.png" => "dya Söng Quynh co'",
        "Screenshot 2026-09-20 104218.png" => "Döi Phong Tiéu",
        "0zoom.png" => "åt Bäng Vinh Ciru",
        "100zoom.png" => "Dåt Bäng Vinh Ciru",
        _ => "",
    }
}

#[test]
#[ignore]
fn e1_zoom_slider_and_map_state_parity() {
    let root = repo_root();
    let img0 = read_image_bgr(&root.join("scratch").join("0zoom.png")).expect("0zoom.png");
    let img100 = read_image_bgr(&root.join("scratch").join("100zoom.png")).expect("100zoom.png");

    let z0 = ZoomSliderDetector::detect_slider(img0.view()).expect("0zoom slider");
    assert!((z0.ratio_t - 0.0).abs() < 1e-4, "z0 ratio_t={}", z0.ratio_t);
    assert!((z0.rho - 0.123644).abs() < 1e-4, "z0 rho={}", z0.rho);
    assert!((z0.scale - 1.924952).abs() < 1e-3, "z0 scale={}", z0.scale);
    assert!(z0.match_score > 0.999 && z0.ccoeff > 0.999);
    assert_eq!(z0.box_coords, [42, 571, 85, 868]);

    let z100 = ZoomSliderDetector::detect_slider(img100.view()).expect("100zoom slider");
    assert!(
        (z100.ratio_t - 0.998887).abs() < 1e-3,
        "z100 ratio_t={}",
        z100.ratio_t
    );
    assert!((z100.scale - 0.320512).abs() < 1e-3, "z100 scale={}", z100.scale);
    assert!(z100.match_score > 0.999 && z100.ccoeff > 0.999);

    let diag_files = collect_diag_game_files(&root);
    let browser_files = collect_diag_browser_files(&root);
    let test_files = collect_test_image_files(&root);
    assert!(diag_files.len() >= 27);
    assert!(browser_files.len() >= 27);
    assert_eq!(test_files.len(), 23);

    let offscreen: HashSet<&str> = ["diag_20261121_011520"].into_iter().collect();
    let open_world: HashSet<&str> = [
        "diag_20261121_205434",
        "diag_20261122_032842",
        "diag_20261122_115106",
        "diag_20261122_121803",
        "diag_20261122_161651",
    ]
    .into_iter()
    .collect();

    let anchors_path = root.join("data").join("ui_anchors_v4.npz");
    let mut sm = MapScreenStateMachine::with_anchors_path(Some(&anchors_path));
    assert!(sm.has_anchor_templates());

    let mut decoded_diags: Vec<(String, Array3<u8>)> = Vec::new();
    for fp in &diag_files {
        let name = diag_name(fp);
        let img = read_image_bgr(fp).expect("diag img");
        let chk = sm.is_map_m_open(Some(img.view()));
        if open_world.contains(name.as_str()) {
            assert!(!chk.is_open, "False Map M open on open-world {name}");
        } else if !offscreen.contains(name.as_str()) {
            assert!(chk.is_open, "Map M missed on {name}: {chk:?}");
        }
        decoded_diags.push((name, img));
    }

    let mut decoded_tests = Vec::new();
    for fp in &test_files {
        let img = read_image_bgr(fp).expect("test img");
        let chk = sm.is_map_m_open(Some(img.view()));
        assert!(chk.is_open, "Map M missed on {:?}", fp.file_name());
        decoded_tests.push((
            fp.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            img,
        ));
    }

    let mut decoded_browsers = Vec::new();
    for fp in &browser_files {
        let img = read_image_bgr(fp).expect("browser img");
        let chk = sm.is_map_m_open(Some(img.view()));
        assert!(
            !chk.is_open && !chk.slider_strict_ok,
            "False positive on browser {fp:?}: {chk:?}"
        );
        decoded_browsers.push(img);
    }

    // Template-free fallback parity (`with_anchors_path(None)`) across all 91 real fixtures
    let mut sm_fallback = MapScreenStateMachine::with_anchors_path(None);
    assert!(!sm_fallback.has_anchor_templates());
    for (name, img) in &decoded_diags {
        let chk = sm_fallback.is_map_m_open(Some(img.view()));
        if open_world.contains(name.as_str()) {
            assert!(
                !chk.is_open,
                "Fallback false Map M open on open-world {name}: {chk:?}"
            );
        } else {
            // Without UI anchors, slider-only fallback opens on map screens (including drawer-covered 011520)
            assert!(chk.is_open, "Fallback Map M missed on {name}: {chk:?}");
        }
    }
    for (name, img) in &decoded_tests {
        let chk = sm_fallback.is_map_m_open(Some(img.view()));
        assert!(chk.is_open, "Fallback Map M missed on {name}: {chk:?}");
    }
    for img in &decoded_browsers {
        let chk = sm_fallback.is_map_m_open(Some(img.view()));
        assert!(
            !chk.is_open && !chk.slider_strict_ok,
            "Fallback false positive on browser: {chk:?}"
        );
    }

    // is_close_x_probe parity: >=25 diags, 1080p, BGRA, 0 FP on browsers
    let mut close_hits = 0usize;
    for (name, img) in &decoded_diags {
        if offscreen.contains(name.as_str()) || open_world.contains(name.as_str()) {
            continue;
        }
        let (h, w, _) = img.dim();
        let rect = MapScreenStateMachine::get_close_probe_rect(0, 0, w as i32, h as i32);
        let roi = img.slice(s![
            rect.y as usize..(rect.y + rect.height) as usize,
            rect.x as usize..(rect.x + rect.width) as usize,
            ..
        ]);
        let (ok, sc) = sm.is_close_x_probe(Some(roi), h, CLOSE_PROBE_DEFAULT_THRESHOLD);
        assert!(ok, "close probe failed on {name}: {sc}");
        close_hits += 1;
    }
    assert!(close_hits >= 25);

    // 1080p + BGRA close probe on first active diag
    let (_, sample_open) = decoded_diags
        .iter()
        .find(|(n, _)| !offscreen.contains(n.as_str()) && !open_world.contains(n.as_str()))
        .expect("active diag");
    let open_1080 = resize_u8_hwc(sample_open.view(), 1920, 1080, Interpolation::Area).unwrap();
    let r1080 = MapScreenStateMachine::get_close_probe_rect(0, 0, 1920, 1080);
    let roi_1080 = open_1080.slice(s![
        r1080.y as usize..(r1080.y + r1080.height) as usize,
        r1080.x as usize..(r1080.x + r1080.width) as usize,
        ..
    ]);
    assert!(
        sm.is_close_x_probe(Some(roi_1080), 1080, CLOSE_PROBE_DEFAULT_THRESHOLD)
            .0
    );
    let bgra_fb = bgr_to_bgra_frame_buf(roi_1080);
    assert!(
        sm.is_close_x_probe_frame(Some(bgra_fb.as_view()), 1080, CLOSE_PROBE_DEFAULT_THRESHOLD)
            .0
    );

    for b_img in &decoded_browsers {
        let (h, w, _) = b_img.dim();
        let rect = MapScreenStateMachine::get_close_probe_rect(0, 0, w as i32, h as i32);
        let roi = b_img.slice(s![
            rect.y as usize..(rect.y + rect.height) as usize,
            rect.x as usize..(rect.x + rect.width) as usize,
            ..
        ]);
        let (ok, sc) = sm.is_close_x_probe(Some(roi), h, CLOSE_PROBE_DEFAULT_THRESHOLD);
        assert!(!ok, "false close probe on browser: {sc}");
    }

    // Sequence transitions + HUD fade-in + drag abort + drawer hysteresis + debounce + scene cut
    let f_closed = &decoded_browsers[0];
    let f_open_a = &decoded_diags[0].1;
    let f_open_b = &decoded_diags[4].1;

    sm.reset();
    let mut fires = Vec::new();
    let mut seq = Vec::new();
    for _ in 0..3 {
        seq.push(f_closed);
    }
    for _ in 0..6 {
        seq.push(f_open_a);
    }
    for _ in 0..4 {
        seq.push(f_closed);
    }
    for _ in 0..5 {
        seq.push(f_open_b);
    }
    for _ in 0..4 {
        seq.push(f_closed);
    }
    for (idx, fr) in seq.into_iter().enumerate() {
        let res = sm.step(Some(fr.view()));
        if res.should_auto_sync {
            fires.push(idx);
        }
    }
    assert_eq!(fires.len(), 2, "fires={fires:?}");
    assert!((3..9).contains(&fires[0]) && (13..18).contains(&fires[1]));

    // HUD fade-in guard
    let (fh, fw, _) = f_open_a.dim();
    let mut f_hud_fade = f_open_a.clone();
    f_hud_fade
        .slice_mut(s![
            (0.86 * fh as f64) as usize..(0.98 * fh as f64) as usize,
            (0.75 * fw as f64) as usize..(0.98 * fw as f64) as usize,
            ..
        ])
        .fill(0);
    sm.reset();
    assert!(!sm.step(Some(f_open_a.view())).should_auto_sync);
    assert!(!sm.step(Some(f_hud_fade.view())).should_auto_sync);
    assert!(!sm.step(Some(f_open_a.view())).should_auto_sync);
    assert!(!sm.step(Some(f_open_a.view())).should_auto_sync);
    assert!(sm.step(Some(f_open_a.view())).should_auto_sync);

    // 2-frame settling drag abort
    let mut f_drag_a = f_open_a.clone();
    f_drag_a
        .slice_mut(s![
            (0.25 * fh as f64) as usize..(0.75 * fh as f64) as usize,
            (0.25 * fw as f64) as usize..(0.75 * fw as f64) as usize,
            ..
        ])
        .fill(200);
    let mut f_drag_b = f_open_a.clone();
    f_drag_b
        .slice_mut(s![
            (0.25 * fh as f64) as usize..(0.75 * fh as f64) as usize,
            (0.25 * fw as f64) as usize..(0.75 * fw as f64) as usize,
            ..
        ])
        .fill(40);
    sm.reset();
    assert_eq!(sm.step(Some(f_open_a.view())).state, MapState::Settling);
    for pf in [&f_drag_a, &f_drag_b, &f_drag_a, &f_drag_b] {
        assert!(!sm.step(Some(pf.view())).should_auto_sync);
    }
    let drag_st = sm.step(Some(f_drag_a.view()));
    assert_eq!(drag_st.state, MapState::Panning);
    assert!(!drag_st.should_auto_sync);

    // Drawer hysteresis + 3-frame close debounce + 1-frame scene cut
    let mut f_drawer = f_open_a.clone();
    f_drawer
        .slice_mut(s![0..(0.11 * fh as f64) as usize, (0.90 * fw as f64) as usize..fw, ..])
        .fill(32);
    let chk_drawer = sm.is_map_m_open(Some(f_drawer.view()));
    assert!(!chk_drawer.is_open && chk_drawer.anchor_count == 2 && chk_drawer.gear_score >= ANCHOR_THRESHOLD);

    let (_, f_011520) = decoded_diags
        .iter()
        .find(|(n, _)| n == "diag_20261121_011520")
        .expect("011520");
    let chk15 = sm.is_map_m_open(Some(f_011520.view()));
    assert!(!chk15.is_open && chk15.slider_strict_ok);

    sm.reset();
    assert_eq!(sm.step(Some(f_drawer.view())).state, MapState::Closed);
    for _ in 0..4 {
        sm.step(Some(f_open_a.view()));
    }
    assert_eq!(sm.state(), MapState::SyncedIdle);
    for _ in 0..3 {
        let h = sm.step(Some(f_drawer.view()));
        assert_eq!(h.state, MapState::SyncedIdle);
        assert!(!h.should_auto_sync);
    }
    for _ in 0..4 {
        let h15 = sm.step(Some(f_011520.view()));
        assert!(matches!(h15.state, MapState::SyncedIdle | MapState::Panning));
        assert!(!h15.should_auto_sync);
    }
    let uncover = sm.step(Some(f_open_a.view()));
    assert!(!uncover.should_auto_sync);

    let mut f_transient = f_open_a.clone();
    f_transient
        .slice_mut(s![0..(0.11 * fh as f64) as usize, (0.90 * fw as f64) as usize..fw, ..])
        .fill(32);
    f_transient
        .slice_mut(s![(0.88 * fh as f64) as usize..fh, 0..(0.09 * fw as f64) as usize, ..])
        .fill(32);
    f_transient
        .slice_mut(s![
            (0.20 * fh as f64) as usize..(0.80 * fh as f64) as usize,
            0..(0.08 * fw as f64) as usize,
            ..
        ])
        .fill(32);
    let d1 = sm.step(Some(f_transient.view()));
    assert_ne!(d1.state, MapState::Closed);
    assert_eq!(d1.scene_cut, Some(false));
    assert!(d1.bhatt_dist.unwrap() < 0.28);
    let d2 = sm.step(Some(f_transient.view()));
    assert_ne!(d2.state, MapState::Closed);
    let d3 = sm.step(Some(f_transient.view()));
    assert_eq!(d3.state, MapState::Closed);
    assert_eq!(d3.scene_cut, Some(false));

    sm.mark_synced(Some(f_open_a.view()));
    let cut = sm.step(Some(f_closed.view()));
    assert_eq!(cut.state, MapState::Closed);
    assert_eq!(cut.scene_cut, Some(true));
    assert!(cut.bhatt_dist.unwrap() >= SCENE_CUT_THRESHOLD);
}

#[test]
#[ignore]
fn e1_heading360_yt_bench_and_minimap_rec_parity() {
    let root = repo_root();
    let live_rec_path = root
        .join("scratch")
        .join("yt_bench")
        .join("cache")
        .join("live_rec.npz");
    let gt_path = root
        .join("scratch")
        .join("yt_bench")
        .join("curate")
        .join("gt.json");

    let mm_npy = NpzArchive::read_single_from_file(&live_rec_path, "mm").expect("read mm.npy");
    let sh = mm_npy.shape();
    assert_eq!(sh.len(), 4);
    let (n_frames, mh, mw, mc) = (sh[0], sh[1], sh[2], sh[3]);
    let mm_data = mm_npy.as_u8().expect("mm u8");
    let mm = ArrayView3::from_shape((n_frames, mh * mw, mc), mm_data).unwrap();
    let _ = mm;
    let frame_at = |idx: usize| -> ArrayView3<'_, u8> {
        let stride = mh * mw * mc;
        ArrayView3::from_shape((mh, mw, mc), &mm_data[idx * stride..(idx + 1) * stride]).unwrap()
    };

    // 1. Curated 43-frame ground truth accuracy & latency
    let gt_raw = std::fs::read_to_string(&gt_path).expect("gt.json");
    let gt_map: BTreeMap<String, f64> = serde_json::from_str(&gt_raw).expect("parse gt.json");
    let mut errs = Vec::new();
    let mut lats_ms = Vec::new();
    let mut ok = 0usize;
    for (k, gt_deg) in &gt_map {
        let idx: usize = k.parse().expect("idx");
        let fr = frame_at(idx);
        let t0 = Instant::now();
        let det = extract_camera_view_cone(fr);
        lats_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        if let Some(est) = det.heading_deg {
            let err = shortest_angle_delta(*gt_deg, est).abs();
            errs.push(err);
            if err <= 20.0 {
                ok += 1;
            }
        }
    }
    errs.sort_by(|a, b| a.total_cmp(b));
    lats_ms.sort_by(|a, b| a.total_cmp(b));
    let med_err = errs[errs.len() / 2];
    let med_lat = lats_ms[lats_ms.len() / 2];
    println!(
        "heading360 curated GT: ok={ok}/{} med_err={med_err:.3}deg med_lat={med_lat:.3}ms",
        gt_map.len()
    );
    assert!(ok >= 42, "curated ok={ok}/{}", gt_map.len());
    assert!(med_err < 2.0, "med_err={med_err}");
    assert!(med_lat < 3.0, "med_lat={med_lat}");

    // 2. eval_live.py 27 LABELS + 18 NEGATIVES
    let labels: &[(usize, f64)] = &[
        (10, 150.0), (90, 100.0), (104, 110.0), (125, 0.0), (128, 340.0),
        (293, 225.0), (436, 220.0), (475, 0.0), (481, 0.0), (491, 135.0),
        (569, 45.0), (652, 130.0), (702, 115.0), (820, 265.0), (859, 80.0),
        (1048, 230.0), (1081, 200.0), (1082, 195.0), (1142, 120.0), (1153, 45.0),
        (1253, 160.0), (1331, 355.0), (1520, 40.0), (1614, 130.0), (1803, 15.0),
        (2086, 225.0), (2181, 230.0),
    ];
    let negatives: &[usize] = &[
        198, 387, 574, 576, 617, 618, 624, 632, 732, 745, 765, 770, 784, 953, 1237, 1425, 1709,
        1992,
    ];
    let mut lab_errs = Vec::new();
    let mut lab_ok = 0usize;
    for &(idx, gt_deg) in labels {
        let det = extract_camera_view_cone(frame_at(idx));
        if let Some(est) = det.heading_deg {
            let e = shortest_angle_delta(gt_deg, est).abs();
            lab_errs.push(e);
            if e <= 25.0 {
                lab_ok += 1;
            }
        }
    }
    lab_errs.sort_by(|a, b| a.total_cmp(b));
    let lab_med = if lab_errs.len() % 2 == 1 {
        lab_errs[lab_errs.len() / 2]
    } else {
        0.5 * (lab_errs[lab_errs.len() / 2 - 1] + lab_errs[lab_errs.len() / 2])
    };
    let neg_out = negatives
        .iter()
        .filter(|&&idx| extract_camera_view_cone(frame_at(idx)).heading_deg.is_some())
        .count();
    println!("heading360 eval_live: ok={lab_ok}/27 med_err={lab_med:.3}deg neg_out={neg_out}/18");
    // Python oracle (`src.heading360.extract_camera_view_cone`) achieves ok=13/27, med_err=21.800deg, neg_out=1/18 on this raw noisy live capture.
    assert!(lab_ok >= 13, "lab_ok={lab_ok}");
    assert!(lab_med < 23.0, "lab_med={lab_med}");
    assert!(neg_out <= 1, "neg_out={neg_out}");
    for &(idx, py_deg) in &[
        (10usize, 147.6f64),
        (859, 82.0),
        (1331, 349.5),
        (1520, 29.5),
        (1614, 122.0),
        (2086, 216.8),
    ] {
        let det = extract_camera_view_cone(frame_at(idx));
        let est = det.heading_deg.unwrap_or_else(|| panic!("frame {idx} missing heading"));
        let diff = shortest_angle_delta(py_deg, est).abs();
        assert!(
            diff <= 6.0,
            "eval_live frame {idx} python={py_deg:.1} rust={est:.1} diff={diff:.2}"
        );
    }

    // 3. minimap_rec/rec_20261122_043401 sampled parity against meta.jsonl cone_raw, latency,
    //    and visually-verified cropped-minimap player arrow ground truth (replaces legacy
    //    hollow-BGR Python oracle values that mis-detected waypoint-overlapped frames by 49-129 deg).
    let rec_dir = root
        .join("scratch")
        .join("minimap_rec")
        .join("rec_20261122_043401");
    let meta_raw = std::fs::read_to_string(rec_dir.join("meta.jsonl")).expect("meta.jsonl");
    let mut rec_errs = Vec::new();
    let mut rec_lats = Vec::new();
    let mut arrow_hits = 0usize;
    let mut sampled = 0usize;
    for line in meta_raw.lines().step_by(20) {
        let v: Value = serde_json::from_str(line).expect("jsonl line");
        let idx = v["i"].as_u64().expect("i") as usize;
        let frame_path = rec_dir.join("frames").join(format!("{idx:06}.png"));
        if !frame_path.is_file() {
            continue;
        }
        let img = read_image_bgr(&frame_path).expect("rec frame");
        let t0 = Instant::now();
        let det = extract_camera_view_cone(img.view());
        rec_lats.push(t0.elapsed().as_secs_f64() * 1000.0);
        if let Some(cone_gt) = v["cone_raw"].as_f64()
            && let Some(est) = det.heading_deg
        {
            rec_errs.push(shortest_angle_delta(cone_gt, est).abs());
            assert_ne!(det.method, ConeMethod::None);
        }
        let mm_crop = img.slice(s![20..311, 79..370, ..]);
        let arrow = extract_player_arrow_360(mm_crop);
        if arrow.heading_deg.is_some() && arrow.confidence >= 0.45 {
            arrow_hits += 1;
        }
        sampled += 1;
    }
    rec_errs.sort_by(|a, b| a.total_cmp(b));
    rec_lats.sort_by(|a, b| a.total_cmp(b));
    let rec_med_err = rec_errs[rec_errs.len() / 2];
    let rec_med_lat = rec_lats[rec_lats.len() / 2];
    println!(
        "heading360 rec_20261122 ({sampled} sampled): med_err={rec_med_err:.3}deg med_lat={rec_med_lat:.3}ms cropped_arrow_hits={arrow_hits}/{sampled}"
    );
    assert!(rec_med_err < 12.0, "rec_med_err={rec_med_err}");
    assert!(rec_med_lat < 3.0, "rec_med_lat={rec_med_lat}");
    // 110 sampled frames contain 38 paimon_pause + 4 gate_hold frames (68 live minimap frames).
    assert!(
        arrow_hits >= 66,
        "cropped minimap arrow hit rate regressed: {arrow_hits}/{sampled}"
    );
    // Visually verified 1440p cropped minimap player arrow ground truth, including frames
    // overlapping teleport waypoints & icons (#241, #406, #1021, #1666) where the legacy
    // hollow-BGR Canny solver flipped by 23-129 deg.
    // #241 was recorded as 7.0 from the old broad-mask solver, which fused the adjacent
    // blue teleport waypoint into the arrow (6.89 deg). The nose/wing geometry, tail jewel
    // and the 60 Hz trajectory f240 28.6 -> f242 9.4 put the true heading at ~19 deg;
    // a solver that re-fuses the waypoint fails here again.
    for &(idx, gt_arrow_deg) in &[
        (1usize, 148.0f64),
        (241, 19.0),
        (406, 32.0),
        (1021, 173.0),
        (1261, 163.0),
        (1481, 148.0),
        (1666, 72.0),
        (2161, 199.0),
        (2181, 199.0),
    ] {
        let p = rec_dir.join("frames").join(format!("{idx:06}.png"));
        let img = read_image_bgr(&p).expect("rec arrow sample frame");
        let mm = img.slice(s![20..311, 79..370, ..]);
        let det = extract_player_arrow_360(mm);
        let est = det
            .heading_deg
            .unwrap_or_else(|| panic!("rec cropped frame {idx} missing arrow heading"));
        let diff = shortest_angle_delta(gt_arrow_deg, est).abs();
        assert!(
            diff <= 8.0,
            "rec_20261122 cropped arrow frame {idx} gt={gt_arrow_deg:.1} rust={est:.2} conf={:.2} diff={diff:.2}",
            det.confidence
        );
    }
    // Ground-truth cropped 1440p minimap samples (`x=79..370, y=20..311`), including both
    // uniform-alpha SDR frames (1, 21, 221, 241) and radial-gradient/cave frames (86, 121, 408)
    // where legacy Python `lut25` failed and drifted via `HsvAdaptive`/`Body`.
    for &(idx, gt_deg) in &[
        (1usize, 148.1f64),
        (21, 147.6),
        (86, 96.3),
        (121, 20.4),
        (221, 79.1),
        (241, 319.9),
        (408, 31.6),
    ] {
        let p = rec_dir.join("frames").join(format!("{idx:06}.png"));
        let img = read_image_bgr(&p).expect("rec sample frame");
        let mm = img.slice(s![20..311, 79..370, ..]);
        let det = extract_camera_view_cone(mm);
        assert_eq!(
            det.method,
            ConeMethod::BgrBlend,
            "rec_20261122 cropped minimap frame {idx} expected trusted BgrBlend, got {:?}",
            det.method
        );
        let est = det
            .heading_deg
            .unwrap_or_else(|| panic!("rec frame {idx} missing heading"));
        let diff = shortest_angle_delta(gt_deg, est).abs();
        assert!(
            diff <= 4.0,
            "rec_20261122 cropped frame {idx} gt={gt_deg:.2} rust={est:.2} diff={diff:.2}"
        );
    }
}

#[test]
#[ignore]
fn e1_locator_v4_suite_and_v5_alignment_parity() {
    let root = repo_root();
    let ocr = Arc::new(FixtureOcrProvider::new());
    let mut loc = MapLocator::with_paths(
        LocatorPaths {
            db_path: Some(root.join("data").join("multimap_v4.json")),
            terrain_npz_path: Some(root.join("data").join("terrain_n2_v4.npz")),
            cave_masks_npz_path: Some(root.join("data").join("cave_masks_v4.npz")),
            ui_anchors_npz_path: Some(root.join("data").join("ui_anchors_v4.npz")),
            legacy_underground_db_path: Some(root.join("data").join("underground_db.json")),
        },
        Some(ocr.clone()),
    );

    let diag_files = collect_diag_game_files(&root);
    let test_files = collect_test_image_files(&root);
    assert!(diag_files.len() >= 27);
    assert_eq!(test_files.len(), 23);

    let offscreen: HashSet<&str> = ["diag_20261121_011520"].into_iter().collect();
    let open_world: HashSet<&str> = [
        "diag_20261121_205434",
        "diag_20261122_032842",
        "diag_20261122_115106",
        "diag_20261122_121803",
        "diag_20261122_161651",
    ]
    .into_iter()
    .collect();
    let terrain_only_diags: HashSet<&str> =
        ["diag_20261120_231651", "diag_20261121_204617"].into_iter().collect();

    let required_locate_keys: &[&str] = &[
        "detected", "has_world_coords", "map_id", "map_name", "area_id", "area_name",
        "world_x", "world_y", "lat", "lng", "pixel_x", "pixel_y", "heading", "scale",
        "inliers_count", "rmse", "is_underground", "in_game_is_underground", "floor_idx",
        "total_floors", "active_floor_idx", "surface_idx", "floor_name", "floor_short",
        "hoyolab_floor_name", "group_id", "group_name", "floor_id", "has_layers", "zoom_ratio_t",
    ];

    let check_schema = |res: &LocateResult| {
        let jv = serde_json::to_value(res).expect("serialize LocateResult");
        let obj = jv.as_object().expect("LocateResult JSON object");
        for &k in required_locate_keys {
            assert!(obj.contains_key(k), "missing required key '{k}' in {obj:?}");
        }
    };

    let mut per_image_ms = Vec::new();
    let mut results_by_name: HashMap<String, LocateResult> = HashMap::new();
    let mut img_cache: HashMap<String, Array3<u8>> = HashMap::new();

    for fp in &diag_files {
        let dname = diag_name(fp);
        let img = read_image_bgr(fp).expect("diag img");
        loc.reset_tracking_state();
        ocr.set(ocr_fixture_text(&dname));

        if offscreen.contains(dname.as_str()) {
            assert!(loc.detect_player_arrow(img.view(), None).is_none());
            assert!(loc.locate_player(img.view(), false, None).is_none());
            img_cache.insert(dname, img);
            continue;
        }
        if open_world.contains(dname.as_str()) {
            assert!(loc.locate_player(img.view(), false, None).is_none());
            img_cache.insert(dname, img);
            continue;
        }

        let t0 = Instant::now();
        let res = loc
            .locate_player(img.view(), true, None)
            .unwrap_or_else(|| panic!("{dname} locate returned None"));
        per_image_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(res.has_world_coords, "{dname} failed world coords: {res:?}");
        check_schema(&res);
        assert!(res.world_x.unwrap().is_finite() && res.world_y.unwrap().is_finite());
        assert!((0.0..360.0).contains(&res.heading));
        let min_inl = if terrain_only_diags.contains(dname.as_str()) { 0 } else { 1 };
        assert!(res.inliers_count.unwrap() >= min_inl, "{dname}: {res:?}");

        // Exact DIAG_ASSERTIONS (all 22)
        match dname.as_str() {
            "diag_20260920_214102" | "diag_20260920_214145" => assert_eq!(res.map_id, Some(36)),
            "diag_20260920_220416" => {
                assert_eq!(res.map_id, Some(40));
                assert!(res.is_underground);
                assert_eq!(res.group_id, Some(212));
                assert_eq!(res.floor_id, Some(306));
            }
            "diag_20260920_220447" => {
                assert_eq!(res.map_id, Some(40));
                assert!(res.is_underground);
                assert_eq!(res.group_id, Some(207));
                assert_eq!(res.floor_id, Some(301));
            }
            "diag_20260920_220225" => {
                assert_eq!((res.map_id, res.area_id, res.group_id, res.floor_id), (Some(2), Some(16), Some(234), Some(347)));
            }
            "diag_20260920_220106" => {
                assert_eq!((res.map_id, res.group_id, res.floor_id), (Some(2), Some(91), Some(81)));
            }
            "diag_20260920_214910" => {
                assert_eq!((res.map_id, res.group_id, res.floor_id), (Some(2), Some(182), Some(249)));
            }
            "diag_20260920_220317" => {
                assert_eq!((res.map_id, res.group_id, res.floor_id), (Some(2), Some(224), Some(325)));
            }
            "diag_20260920_220745" => {
                assert_eq!((res.map_id, res.is_underground, res.group_id, res.floor_id), (Some(2), true, Some(119), Some(150)));
            }
            "diag_20260920_221245" => assert_eq!((res.map_id, res.is_underground), (Some(2), false)),
            "diag_20260920_222052" => {
                assert_eq!((res.map_id, res.group_id, res.floor_id), (Some(2), Some(146), Some(189)));
            }
            "diag_20261120_204445" => assert_eq!((res.map_id, res.area_id, res.is_underground), (Some(2), Some(11), false)),
            "diag_20261120_204655" | "diag_20261120_213109" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground, res.group_id, res.floor_id), (Some(2), Some(8), true, Some(90), Some(78)));
            }
            "diag_20261120_205011" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground, res.group_id, res.floor_id), (Some(2), Some(16), true, Some(235), Some(349)));
            }
            "diag_20261120_231651" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground), (Some(2), Some(16), false));
                assert!((res.world_x.unwrap() - (-5541.9)).abs() <= 15.0);
                assert!((res.world_y.unwrap() - (-7014.9)).abs() <= 15.0);
            }
            "diag_20261120_232016" => assert_eq!((res.map_id, res.area_id, res.is_underground), (Some(2), Some(16), false)),
            "diag_20261120_232232" => assert_eq!((res.map_id, res.area_id, res.is_underground, res.group_id), (Some(2), Some(16), false, None)),
            "diag_20261120_232623" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground, res.group_id, res.floor_id), (Some(2), Some(4), true, Some(26), Some(46)));
            }
            "diag_20261121_011215" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground, res.group_id, res.floor_id), (Some(2), Some(4), true, Some(26), Some(46)));
                assert!((res.world_y.unwrap() - 2208.50).abs() <= 2.5, "011215 wy={:?}", res.world_y);
            }
            "diag_20261121_011322" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground, res.group_id, res.floor_id), (Some(2), Some(8), true, Some(90), Some(78)));
                assert!((res.world_x.unwrap() - (-4942.0)).abs() <= 4.0);
                assert!((res.world_y.unwrap() - (-1478.5)).abs() <= 2.5);
            }
            "diag_20261121_204617" => {
                assert_eq!((res.map_id, res.area_id, res.is_underground), (Some(2), Some(16), false));
                assert!((res.world_x.unwrap() - (-3691.6)).abs() <= 15.0);
                assert!((res.world_y.unwrap() - (-7285.0)).abs() <= 15.0);
            }
            _ => {}
        }

        results_by_name.insert(dname.clone(), res);
        img_cache.insert(dname, img);
    }

    for fp in &test_files {
        let fname = fp.file_name().unwrap().to_string_lossy().into_owned();
        let img = read_image_bgr(fp).expect("test img");
        loc.reset_tracking_state();
        ocr.set(ocr_fixture_text(&fname));

        let t0 = Instant::now();
        let res = loc
            .locate_player(img.view(), true, None)
            .unwrap_or_else(|| panic!("{fname} locate returned None"));
        per_image_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(res.has_world_coords, "{fname} failed world coords: {res:?}");
        check_schema(&res);
        let min_inl = if fname == "Screenshot 2026-09-20 103226.png" {
            assert_eq!(res.map_id, Some(36));
            assert!((res.world_x.unwrap() - 192.1).abs() <= 15.0);
            assert!((res.world_y.unwrap() - 496.9).abs() <= 15.0);
            0
        } else {
            1
        };
        assert!(res.inliers_count.unwrap() >= min_inl, "{fname}");
        results_by_name.insert(fname.clone(), res);
        img_cache.insert(fname, img);
    }

    // Extreme zooms 0zoom vs 100zoom + Verify Alignment [4/6]
    let img0 = read_image_bgr(&root.join("scratch").join("0zoom.png")).unwrap();
    let img100 = read_image_bgr(&root.join("scratch").join("100zoom.png")).unwrap();
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("0zoom.png"));
    let r0 = loc.locate_player(img0.view(), true, None).unwrap();
    assert!(r0.has_world_coords && r0.map_id == Some(2) && r0.area_id == Some(16));
    assert!((1.80..=2.05).contains(&r0.scale.unwrap()));
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("100zoom.png"));
    let r100 = loc.locate_player(img100.view(), true, None).unwrap();
    assert!(r100.has_world_coords && r100.map_id == Some(2) && r100.area_id == Some(16));
    assert!((0.29..=0.35).contains(&r100.scale.unwrap()));
    let dx_z = (r0.world_x.unwrap() - r100.world_x.unwrap()).abs();
    let dy_z = (r0.world_y.unwrap() - r100.world_y.unwrap()).abs();
    let d_z = dx_z.hypot(dy_z);
    assert!(dx_z <= 2.0 && dy_z <= 2.5 && d_z <= 3.0, "0vs100 sobel dx={dx_z} dy={dy_z} d={d_z}");

    // Without terrain_data
    let saved_terrain = loc.terrain_data().clone();
    loc.set_terrain_data(BTreeMap::new());
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("0zoom.png"));
    let r0_nt = loc.locate_player(img0.view(), true, None).unwrap();
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("100zoom.png"));
    let r100_nt = loc.locate_player(img100.view(), true, None).unwrap();
    let dx_nt = (r0_nt.world_x.unwrap() - r100_nt.world_x.unwrap()).abs();
    let dy_nt = (r0_nt.world_y.unwrap() - r100_nt.world_y.unwrap()).abs();
    let d_nt = dx_nt.hypot(dy_nt);
    assert!(dx_nt <= 3.0 && dy_nt <= 3.5 && d_nt <= 4.5, "0vs100 nt dx={dx_nt} dy={dy_nt} d={d_nt}");

    // Underground terrain invariance + 204445 anchor shift + 214102 pre-Sobel
    let r1322_t = &results_by_name["diag_20261121_011322"];
    let r1215_t = &results_by_name["diag_20261121_011215"];
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("diag_20261121_011322"));
    let r1322_nt = loc.locate_player(img_cache["diag_20261121_011322"].view(), true, None).unwrap();
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("diag_20261121_011215"));
    let r1215_nt = loc.locate_player(img_cache["diag_20261121_011215"].view(), true, None).unwrap();
    assert!((r1322_t.world_x.unwrap() - r1322_nt.world_x.unwrap()).abs() < 1e-6);
    assert!((r1322_t.world_y.unwrap() - r1322_nt.world_y.unwrap()).abs() < 1e-6);
    assert!((r1215_t.world_x.unwrap() - r1215_nt.world_x.unwrap()).abs() < 1e-6);
    assert!((r1215_t.world_y.unwrap() - r1215_nt.world_y.unwrap()).abs() < 1e-6);

    let img_2044 = &img_cache["diag_20261120_204445"];
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("diag_20261120_204445"));
    let r_2044_nt = loc.locate_player(img_2044.view(), true, None).unwrap();
    let wps2: HashMap<i64, _> = loc.map_meta(2).unwrap().waypoints.iter().map(|w| (w.id, w)).collect();
    let inl_2044: Vec<_> = r_2044_nt
        .inliers
        .as_ref()
        .unwrap()
        .iter()
        .map(|i| ((i.pixel_x, i.pixel_y, 100), wps2[&i.waypoint_id]))
        .collect();
    let (sc_raw, _, ty_raw, _) = MapLocator::fit_isometric_lsq(&inl_2044).unwrap();
    let wy_raw = sc_raw * r_2044_nt.pixel_y + ty_raw;
    let applied_dy = r_2044_nt.world_y.unwrap() - wy_raw;
    let expected_dy = WORLD_ANCHOR_Y_OFFSET - 5.0 * (img_2044.dim().0 as f64 / 1440.0) * sc_raw;
    assert!((applied_dy - expected_dy).abs() <= 0.5, "applied={applied_dy} expected={expected_dy}");

    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("diag_20260920_214102"));
    let r4102_nt = loc.locate_player(img_cache["diag_20260920_214102"].view(), true, None).unwrap();
    let r4102_t = &results_by_name["diag_20260920_214102"];
    let pre_sobel_4102 = (r4102_t.world_x.unwrap() - r4102_nt.world_x.unwrap())
        .hypot(r4102_t.world_y.unwrap() - r4102_nt.world_y.unwrap());
    assert!(pre_sobel_4102 <= 2.0, "pre_sobel_4102={pre_sobel_4102}");

    loc.set_terrain_data(saved_terrain);

    // Alignment [1/6]: 1440p vs 1080p drift <= 3.0w
    for key in ["diag_20260920_214102", "diag_20261121_011322"] {
        let img_1440 = &img_cache[key];
        let img_1080 = resize_u8_hwc(img_1440.view(), 1920, 1080, Interpolation::Area).unwrap();
        let r1440 = &results_by_name[key];
        loc.reset_tracking_state();
        ocr.set(ocr_fixture_text(key));
        let r1080 = loc.locate_player(img_1080.view(), true, None).unwrap();
        assert_eq!(r1440.map_id, r1080.map_id);
        let drift = (r1440.world_x.unwrap() - r1080.world_x.unwrap())
            .hypot(r1440.world_y.unwrap() - r1080.world_y.unwrap());
        assert!(drift <= 3.0, "{key} 1440 vs 1080 drift={drift}");
    }

    // Alignment [2/6]: Underground Sobel guard + >8w clamp + fog/mismatch reject
    let img_m36 = &img_cache["diag_20260920_214102"];
    let gray_m36 = bgr_to_gray(img_m36.view()).unwrap();
    let wps36: HashMap<i64, _> = loc.map_meta(36).unwrap().waypoints.iter().map(|w| (w.id, w)).collect();
    let inl36: Vec<_> = results_by_name["diag_20260920_214102"]
        .inliers
        .as_ref()
        .unwrap()
        .iter()
        .map(|i| ((i.pixel_x, i.pixel_y, 100), wps36[&i.waypoint_id]))
        .collect();
    let adj36 = MapLocator::compensate_inliers_screen_anchors(&inl36, img_m36.dim().0 as f64 / 1440.0);
    let (sc36, tx36, mut ty36, _) = MapLocator::fit_isometric_lsq(&adj36).unwrap();
    ty36 += WORLD_ANCHOR_Y_OFFSET;
    let (tx_ok, ty_ok, ok_surf) =
        loc.refine_transform_with_terrain(Some(gray_m36.view()), 36, sc36, tx36, ty36, 12, false);
    assert!(ok_surf && (tx_ok - tx36).abs() <= 2.0 && (ty_ok - ty36).abs() <= 2.0);
    let (tx_ug, ty_ug, ok_ug) =
        loc.refine_transform_with_terrain(Some(gray_m36.view()), 36, sc36, tx36, ty36, 12, true);
    assert!(!ok_ug && tx_ug == tx36 && ty_ug == ty36);
    let (tx_far, ty_far, ok_far) =
        loc.refine_transform_with_terrain(Some(gray_m36.view()), 36, sc36, tx36, ty36 + 12.0, 12, false);
    assert!(!ok_far && tx_far == tx36 && ty_far == ty36 + 12.0);

    let r_fog = &results_by_name["diag_20261120_231651"];
    let gray_fog = bgr_to_gray(img_cache["diag_20261120_231651"].view()).unwrap();
    let tx_f = r_fog.world_x.unwrap() - r_fog.scale.unwrap() * r_fog.pixel_x;
    let ty_f = r_fog.world_y.unwrap() - r_fog.scale.unwrap() * r_fog.pixel_y;
    let heavy_fog = gaussian_blur_u8(gray_fog.view(), 51, 51, 15.0, 15.0).unwrap();
    assert!(!loc.refine_transform_with_terrain(Some(heavy_fog.view()), 2, r_fog.scale.unwrap(), tx_f, ty_f, 12, false).2);
    let tx_wrong = -7522.6 - r_fog.scale.unwrap() * r_fog.pixel_x;
    let ty_wrong = -5781.1 - r_fog.scale.unwrap() * r_fog.pixel_y;
    assert!(!loc.refine_transform_with_terrain(Some(gray_fog.view()), 2, r_fog.scale.unwrap(), tx_wrong, ty_wrong, 12, false).2);

    // Alignment [5/6]: Arrow pivot @1440p <= 0.75px, @1080p <= 0.85px, 0zoom/100zoom heading diff <= 2.0
    for key in [
        "diag_20261121_011322",
        "diag_20261121_011215",
        "diag_20261120_231651",
        "diag_20260920_214102",
    ] {
        let img = &img_cache[key];
        let (h, w, _) = img.dim();
        let su = h as f64 / 1440.0;
        let (px, py, hdg) = loc.detect_player_arrow(img.view(), None).unwrap();
        let err = (px - (w - 1) as f64 / 2.0).hypot(py - (h - 1) as f64 / 2.0) / su;
        assert!(err <= 0.75, "{key} arrow pivot err={err}");
        assert!((0.0..360.0).contains(&hdg));

        let img1080 = resize_u8_hwc(img.view(), 1920, 1080, Interpolation::Area).unwrap();
        let (p1080x, p1080y, _) = loc.detect_player_arrow(img1080.view(), None).unwrap();
        let err1080 = (p1080x - 959.5).hypot(p1080y - 539.5) / (1080.0 / 1440.0);
        assert!(err1080 <= 0.85, "{key} arrow1080 err={err1080}");
    }
    let (_, _, h0) = loc.detect_player_arrow(img0.view(), None).unwrap();
    let (_, _, h100) = loc.detect_player_arrow(img100.view(), None).unwrap();
    assert!(shortest_angle_delta(h0, h100).abs() <= 2.0);
    assert!((75.0..=105.0).contains(&h0));

    // Alignment [6/6]: Surface corpus pre-Sobel residuals
    let mut pre_ey_signed = Vec::new();
    let mut all_corpus_entries: Vec<(String, Array3<u8>, LocateResult)> = Vec::new();
    for (k, res) in &results_by_name {
        all_corpus_entries.push((k.clone(), img_cache[k].clone(), res.clone()));
    }
    all_corpus_entries.push(("0zoom.png".to_owned(), img0, r0));
    all_corpus_entries.push(("100zoom.png".to_owned(), img100, r100));

    for (_, img, res) in &all_corpus_entries {
        if res.in_game_is_underground.unwrap_or(false) {
            continue;
        }
        let mid = res.map_id.unwrap();
        let wps: HashMap<i64, _> = loc.map_meta(mid).unwrap().waypoints.iter().map(|w| (w.id, w)).collect();
        let inl: Vec<_> = res
            .inliers
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|i| wps.get(&i.waypoint_id).map(|&w| ((i.pixel_x, i.pixel_y, 100), w)))
            .collect();
        if inl.len() < 2 {
            continue;
        }
        let adj = MapLocator::compensate_inliers_screen_anchors(&inl, img.dim().0 as f64 / 1440.0);
        let (sc, tx, mut ty, _) = MapLocator::fit_isometric_lsq(&adj).unwrap();
        if WORLD_ANCHOR_MAP_IDS.contains(&mid) {
            ty += WORLD_ANCHOR_Y_OFFSET;
        }
        let gray = bgr_to_gray(img.view()).unwrap();
        let (_, ty_r, ok) =
            loc.refine_transform_with_terrain(Some(gray.view()), mid, sc, tx, ty, 12, false);
        if ok {
            pre_ey_signed.push(ty_r - ty);
        }
    }
    assert!(pre_ey_signed.len() >= 17, "accepted={}", pre_ey_signed.len());
    let signed_mean = pre_ey_signed.iter().sum::<f64>() / pre_ey_signed.len() as f64;
    let mut pre_ey_abs: Vec<f64> = pre_ey_signed.iter().map(|x| x.abs()).collect();
    pre_ey_abs.sort_by(|a, b| a.total_cmp(b));
    let med_ey = pre_ey_abs[pre_ey_abs.len() / 2];
    let max_ey = *pre_ey_abs.last().unwrap();
    assert!(signed_mean.abs() <= 1.0, "signed_mean={signed_mean}");
    assert!(med_ey <= 2.5, "med_ey={med_ey}");
    assert!(max_ey <= 8.0, "max_ey={max_ey}");

    // Synthetic 1-WP and 0-WP on diag_20260920_214102
    let base_img = &img_cache["diag_20260920_214102"];
    let truth = &results_by_name["diag_20260920_214102"];
    let keep_px = truth.inliers.as_ref().unwrap()[0].pixel_x.round() as isize;
    let keep_py = truth.inliers.as_ref().unwrap()[0].pixel_y.round() as isize;
    let mut one_wp = base_img.clone();
    let (bh, bw, _) = base_img.dim();
    let paint_circle = |dst: &mut Array3<u8>, cx: isize, cy: isize, r: isize, col: [u8; 3]| {
        for y in (cy - r).max(0)..=(cy + r).min(bh as isize - 1) {
            for x in (cx - r).max(0)..=(cx + r).min(bw as isize - 1) {
                if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r {
                    dst[[y as usize, x as usize, 0]] = col[0];
                    dst[[y as usize, x as usize, 1]] = col[1];
                    dst[[y as usize, x as usize, 2]] = col[2];
                }
            }
        }
    };
    for c in loc.detect_waypoint_candidates(base_img.view(), Some((truth.pixel_x, truth.pixel_y)), None) {
        let cx = c.0.round() as isize;
        let cy = c.1.round() as isize;
        if ((cx - keep_px) as f64).hypot((cy - keep_py) as f64) > 15.0 {
            let sy = (cy - 28).max(0) as usize;
            let sx = (cx - 28).max(0) as usize;
            let col = [base_img[[sy, sx, 0]], base_img[[sy, sx, 1]], base_img[[sy, sx, 2]]];
            paint_circle(&mut one_wp, cx, cy, 22, col);
        }
    }
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("diag_20260920_214102"));
    let r_1wp = loc.locate_player(one_wp.view(), true, None).unwrap();
    assert!(r_1wp.has_world_coords && r_1wp.map_id == truth.map_id);
    let err_1wp = (r_1wp.world_x.unwrap() - truth.world_x.unwrap())
        .hypot(r_1wp.world_y.unwrap() - truth.world_y.unwrap());
    assert!(err_1wp <= 40.0, "err_1wp={err_1wp}");

    let mut zero_wp = one_wp;
    let sy0 = (keep_py - 28).max(0) as usize;
    let sx0 = (keep_px - 28).max(0) as usize;
    let col0 = [base_img[[sy0, sx0, 0]], base_img[[sy0, sx0, 1]], base_img[[sy0, sx0, 2]]];
    paint_circle(&mut zero_wp, keep_px, keep_py, 22, col0);
    loc.reset_tracking_state();
    ocr.set(ocr_fixture_text("diag_20260920_214102"));
    let r_0wp = loc.locate_player(zero_wp.view(), true, None).unwrap();
    assert!(r_0wp.has_world_coords && r_0wp.map_id == truth.map_id);
    let err_0wp = (r_0wp.world_x.unwrap() - truth.world_x.unwrap())
        .hypot(r_0wp.world_y.unwrap() - truth.world_y.unwrap());
    assert!(err_0wp <= 50.0, "err_0wp={err_0wp}");

    // Off-screen cyan-masked arrow rejection on 10 real maps
    let mut offscreen_checked = 0usize;
    for (k, res) in results_by_name.iter().take(10) {
        let mut masked = img_cache[k].clone();
        let (mh, mw, _) = masked.dim();
        let pad = (32.0 * (mh as f64 / 1440.0)) as isize;
        let x1 = (res.pixel_x as isize - pad).max(0) as usize;
        let y1 = (res.pixel_y as isize - pad).max(0) as usize;
        let x2 = (res.pixel_x as isize + pad).min(mw as isize) as usize;
        let y2 = (res.pixel_y as isize + pad).min(mh as isize) as usize;
        for y in y1..y2 {
            for x in x1..x2 {
                masked[[y, x, 0]] = 210;
                masked[[y, x, 1]] = 175;
                masked[[y, x, 2]] = 85;
            }
        }
        assert!(loc.detect_player_arrow(masked.view(), None).is_none());
        assert!(loc.locate_player(masked.view(), false, None).is_none());
        offscreen_checked += 1;
    }
    assert_eq!(offscreen_checked, 10);

    // Cold & warm latency budget
    per_image_ms.sort_by(|a, b| a.total_cmp(b));
    let cold_med = per_image_ms[per_image_ms.len() / 2];
    let cold_max = *per_image_ms.last().unwrap();
    let warm_img = &img_cache["diag_20261121_011322"];
    ocr.set(ocr_fixture_text("diag_20261121_011322"));
    let _ = loc.locate_player(warm_img.view(), false, None);
    let t_warm0 = Instant::now();
    let warm_res = loc.locate_player(warm_img.view(), false, None).unwrap();
    let warm_ms = t_warm0.elapsed().as_secs_f64() * 1000.0;
    assert!(warm_res.has_world_coords);
    println!(
        "locator corpus ({} imgs): cold_med={cold_med:.2}ms cold_max={cold_max:.2}ms warm={warm_ms:.2}ms | pre_sobel_cnt={} signed_mean={signed_mean:+.3} med_ey={med_ey:.3}",
        per_image_ms.len(),
        pre_ey_signed.len()
    );
    assert!(cold_med < 150.0, "cold_med={cold_med}");
    assert!(cold_max < 400.0, "cold_max={cold_max}");
    assert!(warm_ms < 180.0, "warm_ms={warm_ms}");
}

#[test]
#[ignore]
fn e1_universal_layer_detector_matches_template_oracle_without_ui_anchors() {
    let root = repo_root();
    let tpl_loc = MapLocator::with_paths(
        LocatorPaths {
            db_path: Some(root.join("data").join("multimap_v4.json")),
            terrain_npz_path: Some(root.join("data").join("terrain_n2_v4.npz")),
            cave_masks_npz_path: Some(root.join("data").join("cave_masks_v4.npz")),
            ui_anchors_npz_path: Some(root.join("data").join("ui_anchors_v4.npz")),
            legacy_underground_db_path: None,
        },
        None,
    );
    let univ_loc = MapLocator::with_paths(
        LocatorPaths {
            db_path: Some(root.join("data").join("multimap_v4.json")),
            terrain_npz_path: Some(root.join("data").join("terrain_n2_v4.npz")),
            cave_masks_npz_path: Some(root.join("data").join("cave_masks_v4.npz")),
            ui_anchors_npz_path: Some(root.join("data").join("__nonexistent_ui_anchors__.npz")),
            legacy_underground_db_path: None,
        },
        None,
    );
    assert!(!tpl_loc.ui_anchors().is_empty());
    assert!(univ_loc.ui_anchors().is_empty());

    let mut files = collect_diag_game_files(&root);
    files.extend(collect_test_image_files(&root));
    if let Ok(appdata) = std::env::var("APPDATA") {
        let snez_diag = PathBuf::from(appdata)
            .join("GenshinHelper")
            .join("diagnostics")
            .join("diag_20261126_023726")
            .join("01_game.png");
        if snez_diag.is_file() {
            files.push(snez_diag);
        }
    }
    assert!(files.len() >= 50, "expected >= 50 fixtures, got {}", files.len());

    let mut checked = 0usize;
    let mut layered = 0usize;
    let mut latencies_ms = Vec::with_capacity(files.len());
    for fp in &files {
        let img = read_image_bgr(fp).expect("fixture img");
        let oracle = tpl_loc.detect_layers_universal(img.view());
        let t0 = Instant::now();
        let got = univ_loc.detect_layers_universal(img.view());
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        assert_eq!(
            got.has_layers, oracle.has_layers,
            "{}: has_layers mismatch: got={got:?} oracle={oracle:?}",
            fp.display()
        );
        if oracle.has_layers {
            layered += 1;
            assert_eq!(
                (got.active_floor_idx, got.total_floors, got.surface_idx, got.is_underground),
                (
                    oracle.active_floor_idx,
                    oracle.total_floors,
                    oracle.surface_idx,
                    oracle.is_underground
                ),
                "{}: layer state mismatch: got={got:?} oracle={oracle:?}",
                fp.display()
            );
        }
        checked += 1;
    }

    latencies_ms.sort_by(|a, b| a.total_cmp(b));
    let med_ms = latencies_ms[latencies_ms.len() / 2];
    eprintln!(
        "e1_universal_layer_detector PASS (checked={checked}, layered={layered}, med_lat={med_ms:.2}ms)"
    );
    assert!(med_ms < 5.0, "universal layer detector too slow: {med_ms:.2}ms");
}
