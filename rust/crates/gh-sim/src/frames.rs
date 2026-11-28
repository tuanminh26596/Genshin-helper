//! Data-driven still-frame regression fixtures (`gh-sim frames`).
//!
//! Validates:
//! - Production [`gh_core::ui_layout::compute_ui_layout`] and the tracker HUD crop formula
//!   against real user-reported screenshots.
//! - Production [`gh_cv::locator::MapLocator::locate_player_frame`] (original + deterministic
//!   +/-2 LSB perturbed variants) against recorded M-Map screenshots.
//! - Production [`gh_cv::global_relocator::TeyvatGlobalRelocator`] against cropped minimap
//!   frames decoded from recorded QA video sessions (`session_frame` + `global_relocate`).
//!
//! Screenshots and QA videos live outside git (`scratch/`). Missing media, missing map datasets,
//! or omitted `--ffmpeg` for video fixtures are reported as `skipped` (never `pass`).

use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::traits::OcrProvider;
pub use gh_core::ui_layout::compute_tracker_hud_crop;
use gh_core::ui_layout::compute_ui_layout;
use gh_cv::global_relocator::{
    DEFAULT_MIN_BIN_SCORE, DEFAULT_TOP_K, GLOBAL_INDEX_FILENAME, TeyvatGlobalRelocator, VoteBins,
    load_map2_raster_from_npz,
};
use gh_cv::locator::{LocateResult, LocatorPaths, MapLocator};
use gh_win32::win_ocr::WindowsOcrService;
use image::ImageReader;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const FRAME_FIXTURE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_FRAME_FIXTURES_DIR: &str = "tests/frame_fixtures";
pub const DEFAULT_DATA_DIR: &str = "data";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("gh_sim_frames_{prefix}_{pid}_{n}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self { path: dir }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl FixtureRect {
    fn from_rect_i32(r: RectI32) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

impl fmt::Display for FixtureRect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{},{}x{})", self.x, self.y, self.width, self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCircle {
    pub cx: f64,
    pub cy: f64,
    pub radius: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrevWorldHint {
    pub map_id: i32,
    pub world_x: f64,
    pub world_y: f64,
    /// Repo-relative anchor frame used to prime `MapLocator`'s internal
    /// `last_known_pos`/`last_map_id` state via public `locate_player_frame`.
    pub prime_image: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MMapLocateExpectation {
    pub expected_map_id: i32,
    pub expected_world_x: f64,
    pub expected_world_y: f64,
    pub pos_tolerance_w: f64,
    #[serde(default)]
    pub prev_hint: Option<PrevWorldHint>,
    #[serde(default)]
    pub perturb_seeds: Vec<u64>,
}

fn default_true() -> bool {
    true
}

fn default_min_bin_score() -> f64 {
    DEFAULT_MIN_BIN_SCORE
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFrameSource {
    pub session: String,
    pub frame: usize,
    #[serde(default = "default_true")]
    pub crop_minimap: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalRelocateExpectation {
    #[serde(default)]
    pub expected_world_x: Option<f64>,
    #[serde(default)]
    pub expected_world_y: Option<f64>,
    #[serde(default)]
    pub pos_tolerance_w: Option<f64>,
    #[serde(default = "default_min_bin_score")]
    pub min_bin_score: f64,
    #[serde(default)]
    pub min_votes_near_target: usize,
    #[serde(default)]
    pub expect_no_match: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameFixture {
    pub schema_version: u32,
    pub name: String,
    pub description: String,
    pub notes: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub session_frame: Option<SessionFrameSource>,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub is_gamepad: bool,
    #[serde(default)]
    pub known_bug: Option<String>,
    #[serde(default)]
    pub expected_minimap_rect: Option<FixtureRect>,
    #[serde(default)]
    pub expected_paimon_rect: Option<FixtureRect>,
    #[serde(default)]
    pub tolerance_px: Option<i32>,
    #[serde(default)]
    pub true_minimap_circle: Option<FixtureCircle>,
    #[serde(default = "default_true")]
    pub expect_circle_in_crop: bool,
    #[serde(default)]
    pub mmap_locate: Option<MMapLocateExpectation>,
    #[serde(default)]
    pub global_relocate: Option<GlobalRelocateExpectation>,
}

impl FrameFixture {
    pub fn has_layout_check(&self) -> bool {
        self.expected_minimap_rect.is_some()
            && self.expected_paimon_rect.is_some()
            && self.tolerance_px.is_some()
            && self.true_minimap_circle.is_some()
    }

    pub fn validate(&self, source_label: &str) -> Result<(), String> {
        if self.schema_version != FRAME_FIXTURE_SCHEMA_VERSION {
            return Err(format!(
                "{source_label}: unsupported schema_version {} (expected {FRAME_FIXTURE_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        if self.name.trim().is_empty() {
            return Err(format!("{source_label}: name must be non-empty"));
        }
        match (&self.image, &self.session_frame) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{source_label}: specify either image or session_frame, not both"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "{source_label}: fixture must specify either image or session_frame"
                ));
            }
            (Some(img), None) => {
                if img.trim().is_empty() {
                    return Err(format!("{source_label}: image must be non-empty"));
                }
                let img_path = Path::new(img);
                if img_path.is_absolute()
                    || img_path
                        .components()
                        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
                {
                    return Err(format!(
                        "{source_label}: image must be a safe repo-relative path, got '{img}'"
                    ));
                }
            }
            (None, Some(sf)) => {
                if sf.session.trim().is_empty() {
                    return Err(format!(
                        "{source_label}: session_frame.session must be non-empty"
                    ));
                }
                let mut comps = Path::new(&sf.session).components();
                match (comps.next(), comps.next()) {
                    (Some(Component::Normal(_)), None) => {}
                    _ => {
                        return Err(format!(
                            "{source_label}: session_frame.session must be a safe single directory name, got '{}'",
                            sf.session
                        ));
                    }
                }
                if sf.frame == 0 {
                    return Err(format!(
                        "{source_label}: session_frame.frame is 1-based and must be >= 1"
                    ));
                }
            }
        }
        if self.width == 0 || self.height == 0 {
            return Err(format!(
                "{source_label}: invalid fixture dimensions {}x{}",
                self.width, self.height
            ));
        }

        let layout_field_count = usize::from(self.expected_minimap_rect.is_some())
            + usize::from(self.expected_paimon_rect.is_some())
            + usize::from(self.tolerance_px.is_some())
            + usize::from(self.true_minimap_circle.is_some());
        if layout_field_count != 0 && layout_field_count != 4 {
            return Err(format!(
                "{source_label}: layout fields (expected_minimap_rect, expected_paimon_rect, tolerance_px, true_minimap_circle) must all be present or all omitted"
            ));
        }
        let has_layout = layout_field_count == 4;
        if !has_layout && self.mmap_locate.is_none() && self.global_relocate.is_none() {
            return Err(format!(
                "{source_label}: fixture must specify at least one check mode (layout fields, mmap_locate, or global_relocate)"
            ));
        }
        if let Some(sf) = &self.session_frame
            && sf.crop_minimap
            && (has_layout || self.mmap_locate.is_some())
        {
            return Err(format!(
                "{source_label}: session_frame with crop_minimap=true only supports global_relocate"
            ));
        }

        if has_layout {
            let tol = self.tolerance_px.unwrap();
            let mm = self.expected_minimap_rect.unwrap();
            let pm = self.expected_paimon_rect.unwrap();
            let c = self.true_minimap_circle.unwrap();
            if tol < 0 {
                return Err(format!(
                    "{source_label}: tolerance_px must be >= 0, got {tol}"
                ));
            }
            if mm.width <= 0 || mm.height <= 0 {
                return Err(format!(
                    "{source_label}: expected_minimap_rect must have positive size, got {mm}"
                ));
            }
            if pm.width <= 0 || pm.height <= 0 {
                return Err(format!(
                    "{source_label}: expected_paimon_rect must have positive size, got {pm}"
                ));
            }
            if !c.cx.is_finite() || !c.cy.is_finite() || !c.radius.is_finite() || c.radius <= 0.0 {
                return Err(format!(
                    "{source_label}: true_minimap_circle must be finite with radius > 0, got ({}, {}, r={})",
                    c.cx, c.cy, c.radius
                ));
            }
        }

        if let Some(m) = &self.mmap_locate {
            if !m.expected_world_x.is_finite() || !m.expected_world_y.is_finite() {
                return Err(format!(
                    "{source_label}: mmap_locate expected_world_x/y must be finite, got ({}, {})",
                    m.expected_world_x, m.expected_world_y
                ));
            }
            if !m.pos_tolerance_w.is_finite() || m.pos_tolerance_w <= 0.0 {
                return Err(format!(
                    "{source_label}: mmap_locate pos_tolerance_w must be finite and > 0, got {}",
                    m.pos_tolerance_w
                ));
            }
            if let Some(hint) = &m.prev_hint {
                if !hint.world_x.is_finite() || !hint.world_y.is_finite() {
                    return Err(format!(
                        "{source_label}: mmap_locate.prev_hint world_x/y must be finite, got ({}, {})",
                        hint.world_x, hint.world_y
                    ));
                }
                if hint.prime_image.trim().is_empty() {
                    return Err(format!(
                        "{source_label}: mmap_locate.prev_hint.prime_image must be non-empty"
                    ));
                }
                let p_path = Path::new(&hint.prime_image);
                if p_path.is_absolute()
                    || p_path
                        .components()
                        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
                {
                    return Err(format!(
                        "{source_label}: mmap_locate.prev_hint.prime_image must be a safe repo-relative path, got '{}'",
                        hint.prime_image
                    ));
                }
            }
        }

        if let Some(g) = &self.global_relocate {
            if !g.min_bin_score.is_finite() || g.min_bin_score <= 0.0 {
                return Err(format!(
                    "{source_label}: global_relocate.min_bin_score must be finite and > 0, got {}",
                    g.min_bin_score
                ));
            }
            if g.expect_no_match {
                if let Some(tol) = g.pos_tolerance_w
                    && (!tol.is_finite() || tol <= 0.0)
                {
                    return Err(format!(
                        "{source_label}: global_relocate.pos_tolerance_w must be finite and > 0, got {tol}"
                    ));
                }
            } else {
                let (Some(wx), Some(wy)) = (g.expected_world_x, g.expected_world_y) else {
                    return Err(format!(
                        "{source_label}: global_relocate requires expected_world_x and expected_world_y when expect_no_match is false"
                    ));
                };
                if !wx.is_finite() || !wy.is_finite() {
                    return Err(format!(
                        "{source_label}: global_relocate expected_world_x/y must be finite, got ({wx}, {wy})"
                    ));
                }
                let Some(tol) = g.pos_tolerance_w else {
                    return Err(format!(
                        "{source_label}: global_relocate requires pos_tolerance_w when expect_no_match is false"
                    ));
                };
                if !tol.is_finite() || tol <= 0.0 {
                    return Err(format!(
                        "{source_label}: global_relocate.pos_tolerance_w must be finite and > 0, got {tol}"
                    ));
                }
            }
        }

        if let Some(bug) = &self.known_bug
            && bug.trim().is_empty()
        {
            return Err(format!(
                "{source_label}: known_bug must be omitted/null or a non-empty string"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameOutcome {
    Pass,
    Fail,
    KnownFail,
    Xpass,
    Skipped,
}

impl FrameOutcome {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::KnownFail => "KNOWN-FAIL",
            Self::Xpass => "XPASS",
            Self::Skipped => "SKIPPED",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameCheckResult {
    pub name: String,
    pub fixture_path: String,
    pub resolved_image: Option<String>,
    pub outcome: FrameOutcome,
    pub known_bug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub failures: Vec<String>,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FramesReport {
    pub pass: usize,
    pub fail: usize,
    pub known_fail: usize,
    pub xpass: usize,
    pub skipped: usize,
    pub results: Vec<FrameCheckResult>,
}

impl FramesReport {
    pub fn summary_line(&self) -> String {
        format!(
            "Frames: pass {} | fail {} | known-fail {} | xpass {} | skipped {}",
            self.pass, self.fail, self.known_fail, self.xpass, self.skipped
        )
    }
}

#[derive(Debug, Clone)]
pub struct FramesOptions {
    pub dir: PathBuf,
    pub repo_root: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub sessions_dir: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
}

impl Default for FramesOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::from(DEFAULT_FRAME_FIXTURES_DIR),
            repo_root: None,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            sessions_dir: None,
            ffmpeg_path: None,
        }
    }
}

#[inline]
fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Generates a deterministic perturbed copy of a BGRA8 frame by adding independent
/// pseudo-random `[-2, +2]` LSB noise to each B, G, R channel (saturating) and
/// preserving `alpha = 255`.
pub fn perturb_bgra_frame(src: &FrameBuf, seed: u64) -> FrameBuf {
    debug_assert_eq!(src.format, PixelFormat::Bgra8);
    let mut out = FrameBuf::new_zeroed(src.width, src.height, PixelFormat::Bgra8);
    let mut state = seed;
    for (sp, dp) in src
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.data.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = splitmix64_next(&mut state);
        let db = ((r & 0xFFFF) % 5) as i8 - 2;
        let dg = (((r >> 16) & 0xFFFF) % 5) as i8 - 2;
        let dr = (((r >> 32) & 0xFFFF) % 5) as i8 - 2;
        dp[0] = sp[0].saturating_add_signed(db);
        dp[1] = sp[1].saturating_add_signed(dg);
        dp[2] = sp[2].saturating_add_signed(dr);
        dp[3] = 255;
    }
    out
}

fn load_png_as_bgra(path: &Path) -> Result<FrameBuf, String> {
    let dyn_img = ImageReader::open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?
        .decode()
        .map_err(|e| format!("failed to decode {}: {e}", path.display()))?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = FrameBuf::new_zeroed(w, h, PixelFormat::Bgra8);
    for (src, dst) in rgba
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.data.as_chunks_mut::<4>().0.iter_mut())
    {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    Ok(out)
}

fn resolve_main_checkout(repo_root: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

fn resolve_fixture_image(
    repo_root: &Path,
    main_checkout: Option<&Path>,
    rel_image: &str,
) -> Option<PathBuf> {
    let local = repo_root.join(rel_image);
    if local.is_file() {
        return Some(local);
    }
    if let Some(main_root) = main_checkout {
        let candidate = main_root.join(rel_image);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn has_required_locator_datasets(dir: &Path) -> bool {
    gh_core::assets::REQUIRED_ASSETS
        .iter()
        .all(|name| dir.join(name).is_file())
}

fn has_terrain_n2_dataset(dir: &Path) -> bool {
    dir.join("terrain_n2_v4.npz").is_file()
}

fn resolve_data_dir_with_check(
    repo_root: &Path,
    main_checkout: Option<&Path>,
    data_dir: &Path,
    pred: fn(&Path) -> bool,
) -> Option<PathBuf> {
    if data_dir.is_absolute() {
        return pred(data_dir).then(|| data_dir.to_path_buf());
    }
    let local = repo_root.join(data_dir);
    if pred(&local) {
        return Some(local);
    }
    if let Some(main_root) = main_checkout {
        let candidate = main_root.join(data_dir);
        if pred(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_data_dir(
    repo_root: &Path,
    main_checkout: Option<&Path>,
    data_dir: &Path,
) -> Option<PathBuf> {
    resolve_data_dir_with_check(repo_root, main_checkout, data_dir, has_required_locator_datasets)
}

fn resolve_sessions_dir(
    repo_root: &Path,
    main_checkout: Option<&Path>,
    sessions_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(sdir) = sessions_dir {
        if sdir.is_absolute() {
            return sdir.is_dir().then(|| sdir.to_path_buf());
        }
        let local = repo_root.join(sdir);
        if local.is_dir() {
            return Some(local);
        }
        if let Some(main_root) = main_checkout {
            let candidate = main_root.join(sdir);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        return None;
    }
    let local = repo_root.join("scratch").join("qa_sessions");
    if local.is_dir() {
        return Some(local);
    }
    if let Some(main_root) = main_checkout {
        let candidate = main_root.join("scratch").join("qa_sessions");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct BatchCropKey {
    video_path: PathBuf,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DecodedFrameKey {
    batch: BatchCropKey,
    frame_1based: usize,
}

fn decode_video_crop_batch(
    ffmpeg_abs: &Path,
    batch: &BatchCropKey,
    frames_1based: &BTreeSet<usize>,
) -> Result<HashMap<DecodedFrameKey, FrameBuf>, String> {
    if !ffmpeg_abs.is_absolute() || !ffmpeg_abs.is_file() {
        return Err(format!(
            "--ffmpeg must be an existing absolute file path (never resolved from PATH): {}",
            ffmpeg_abs.display()
        ));
    }
    if frames_1based.is_empty() {
        return Ok(HashMap::new());
    }
    let select_expr = frames_1based
        .iter()
        .map(|&f| format!("eq(n,{})", f - 1))
        .collect::<Vec<_>>()
        .join("+");
    let vf = format!(
        "select='{select_expr}',crop={}:{}:{}:{}",
        batch.w, batch.h, batch.x, batch.y
    );
    let count = frames_1based.len();
    let count_str = count.to_string();
    let output = Command::new(ffmpeg_abs)
        .args(["-v", "error", "-i"])
        .arg(&batch.video_path)
        .args([
            "-vf",
            &vf,
            "-vsync",
            "0",
            "-frames:v",
            &count_str,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgr24",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to launch ffmpeg ({}): {e}", ffmpeg_abs.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffmpeg frame decode failed on '{}' (exit {:?}): {}",
            batch.video_path.display(),
            output.status.code(),
            stderr.trim()
        ));
    }
    let w = batch.w as usize;
    let h = batch.h as usize;
    let frame_bytes = w
        .checked_mul(h)
        .and_then(|px| px.checked_mul(3))
        .ok_or_else(|| "decoded frame byte size overflow".to_owned())?;
    let expected_bytes = count * frame_bytes;
    if output.stdout.len() != expected_bytes {
        return Err(format!(
            "ffmpeg decoded {} bytes from '{}' (expected {expected_bytes} bytes for {count} frame(s) of {w}x{h})",
            output.stdout.len(),
            batch.video_path.display()
        ));
    }
    let mut decoded = HashMap::with_capacity(count);
    for (idx, &f_1based) in frames_1based.iter().enumerate() {
        let start = idx * frame_bytes;
        let end = start + frame_bytes;
        let mut buf = FrameBuf::new_zeroed(batch.w as u32, batch.h as u32, PixelFormat::Bgr8);
        buf.data.copy_from_slice(&output.stdout[start..end]);
        decoded.insert(
            DecodedFrameKey {
                batch: batch.clone(),
                frame_1based: f_1based,
            },
            buf,
        );
    }
    Ok(decoded)
}

fn build_frame_locator(data_dir: &Path) -> MapLocator {
    let ocr = WindowsOcrService::try_new()
        .ok()
        .map(|s| Arc::new(s) as Arc<dyn OcrProvider>);
    let [db, terrain, cave_masks] = gh_core::assets::REQUIRED_ASSETS;
    let [ui_anchors] = gh_core::assets::OPTIONAL_ASSETS;
    MapLocator::with_paths(
        LocatorPaths {
            db_path: Some(data_dir.join(db)),
            terrain_npz_path: Some(data_dir.join(terrain)),
            cave_masks_npz_path: Some(data_dir.join(cave_masks)),
            ui_anchors_npz_path: Some(data_dir.join(ui_anchors)),
            legacy_underground_db_path: None,
        },
        ocr,
    )
}

fn classify_outcome(all_passed: bool, has_known_bug: bool) -> FrameOutcome {
    match (all_passed, has_known_bug) {
        (true, false) => FrameOutcome::Pass,
        (false, false) => FrameOutcome::Fail,
        (false, true) => FrameOutcome::KnownFail,
        (true, true) => FrameOutcome::Xpass,
    }
}

fn check_rect_within_tol(
    label: &str,
    actual: FixtureRect,
    expected: FixtureRect,
    tol: i32,
    failures: &mut Vec<String>,
) {
    let dx = (actual.x - expected.x).abs();
    let dy = (actual.y - expected.y).abs();
    let dw = (actual.width - expected.width).abs();
    let dh = (actual.height - expected.height).abs();
    if dx > tol || dy > tol || dw > tol || dh > tol {
        failures.push(format!(
            "{label} mismatch: got {actual}, expected {expected} (dx={dx}, dy={dy}, dw={dw}, dh={dh}, tol={tol})"
        ));
    }
}

fn check_circle_inside_box(
    box_label: &str,
    circle: FixtureCircle,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    failures: &mut Vec<String>,
) {
    let c_left = circle.cx - circle.radius;
    let c_right = circle.cx + circle.radius;
    let c_top = circle.cy - circle.radius;
    let c_bottom = circle.cy + circle.radius;
    let mut clips = Vec::new();
    if c_left < left {
        clips.push(format!("left clipped by {:.1}px ({c_left:.1} < {left:.1})", left - c_left));
    }
    if c_right > right {
        clips.push(format!("right clipped by {:.1}px ({c_right:.1} > {right:.1})", c_right - right));
    }
    if c_top < top {
        clips.push(format!("top clipped by {:.1}px ({c_top:.1} < {top:.1})", top - c_top));
    }
    if c_bottom > bottom {
        clips.push(format!("bottom clipped by {:.1}px ({c_bottom:.1} > {bottom:.1})", c_bottom - bottom));
    }
    if !clips.is_empty() {
        failures.push(format!(
            "true_minimap_circle ({:.1},{:.1},r={:.1}) outside {box_label} [{left:.0},{right:.0}]x[{top:.0},{bottom:.0}]: {}",
            circle.cx,
            circle.cy,
            circle.radius,
            clips.join("; ")
        ));
    }
}

fn check_locate_result(
    variant_label: &str,
    res: Option<&LocateResult>,
    exp: &MMapLocateExpectation,
    failures: &mut Vec<String>,
) {
    let Some(r) = res else {
        failures.push(format!(
            "mmap_locate[{variant_label}]: locate_player_frame returned None (arrow not detected); exp map_id={} ({:.1}, {:.1}) tol={:.1}w",
            exp.expected_map_id, exp.expected_world_x, exp.expected_world_y, exp.pos_tolerance_w
        ));
        return;
    };
    let inliers_str = r
        .inliers_count
        .map(|n| n.to_string())
        .unwrap_or_else(|| "none".to_owned());
    if !r.has_world_coords {
        failures.push(format!(
            "mmap_locate[{variant_label}]: map_id={:?}, pos=none, inliers={inliers_str}, has_world_coords=false (detected={}); exp map_id={} ({:.1}, {:.1}) tol={:.1}w",
            r.map_id, r.detected, exp.expected_map_id, exp.expected_world_x, exp.expected_world_y, exp.pos_tolerance_w
        ));
        return;
    }
    let (actual_x, actual_y) = match (r.world_x, r.world_y) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => (x, y),
        _ => {
            failures.push(format!(
                "mmap_locate[{variant_label}]: map_id={:?}, non-finite world coords ({:?}, {:?}), inliers={inliers_str}, has_world_coords={}",
                r.map_id, r.world_x, r.world_y, r.has_world_coords
            ));
            return;
        }
    };
    let dist_w = (actual_x - exp.expected_world_x).hypot(actual_y - exp.expected_world_y);
    let map_ok = r.map_id == Some(exp.expected_map_id);
    let pos_ok = dist_w <= exp.pos_tolerance_w;
    if !map_ok || !pos_ok {
        failures.push(format!(
            "mmap_locate[{variant_label}]: map_id={:?} (exp {}), pos=({actual_x:.1}, {actual_y:.1}) vs ({:.1}, {:.1}), dist_w={dist_w:.1} (tol {:.1}), inliers={inliers_str}, has_world_coords={}",
            r.map_id,
            exp.expected_map_id,
            exp.expected_world_x,
            exp.expected_world_y,
            exp.pos_tolerance_w,
            r.has_world_coords
        ));
    }
}

fn check_global_relocate_result(
    reloc: &TeyvatGlobalRelocator,
    frame: &FrameBuf,
    exp: &GlobalRelocateExpectation,
    failures: &mut Vec<String>,
) -> String {
    let mut bins = VoteBins::new();
    let cands = reloc.query_candidates_frame(
        frame.as_view(),
        Some(&mut bins),
        DEFAULT_TOP_K,
        exp.min_bin_score,
    );
    let total_votes: usize = bins.values().map(Vec::len).sum();
    let num_des = reloc.num_descriptors();

    if exp.expect_no_match {
        let top_score = cands.first().map_or(0.0, |c| c.score);
        let detail = format!(
            "des={num_des} cands={} total_votes={total_votes} top_score={top_score:.1}",
            cands.len()
        );
        if let Some(top) = cands.first() {
            failures.push(format!(
                "global_relocate[expect_no_match]: unexpected match at ({:.1}, {:.1}) ({detail})",
                top.world_x, top.world_y
            ));
        }
        return detail;
    }

    let tx = exp.expected_world_x.expect("validated expected_world_x");
    let ty = exp.expected_world_y.expect("validated expected_world_y");
    let tol = exp.pos_tolerance_w.expect("validated pos_tolerance_w");
    let votes_near = bins
        .values()
        .flatten()
        .filter(|v| (v.world_x - tx).hypot(v.world_y - ty) <= tol)
        .count();

    let detail = if let Some(top) = cands.first() {
        let dist_w = (top.world_x - tx).hypot(top.world_y - ty);
        format!(
            "des={num_des} votes_near={votes_near}/{total_votes} top_score={:.1} best=({:.1},{:.1}) dist={dist_w:.1}w",
            top.score, top.world_x, top.world_y
        )
    } else {
        format!(
            "des={num_des} votes_near={votes_near}/{total_votes} cands=0 (min_score={:.1})",
            exp.min_bin_score
        )
    };

    match cands.first() {
        None => {
            failures.push(format!(
                "global_relocate: 0 candidates ({detail}; exp=({tx:.1},{ty:.1}) tol={tol:.1}w)"
            ));
        }
        Some(top) => {
            let dist_w = (top.world_x - tx).hypot(top.world_y - ty);
            if dist_w > tol {
                failures.push(format!(
                    "global_relocate: top candidate out of tolerance ({detail}; exp=({tx:.1},{ty:.1}) tol={tol:.1}w)"
                ));
            }
        }
    }
    if votes_near < exp.min_votes_near_target {
        failures.push(format!(
            "global_relocate: votes_near {votes_near} < min_votes_near_target {} ({detail})",
            exp.min_votes_near_target
        ));
    }

    detail
}

fn build_skipped_result(
    fixture: &FrameFixture,
    fixture_path_str: String,
    resolved_image: Option<String>,
    skip_reason: String,
) -> FrameCheckResult {
    FrameCheckResult {
        name: fixture.name.clone(),
        fixture_path: fixture_path_str,
        resolved_image,
        outcome: FrameOutcome::Skipped,
        known_bug: fixture.known_bug.clone(),
        detail: None,
        failures: Vec::new(),
        skip_reason: Some(skip_reason),
    }
}

pub fn evaluate_fixture(
    fixture: &FrameFixture,
    fixture_path: &Path,
    repo_root: &Path,
    main_checkout: Option<&Path>,
    locator: Option<&MapLocator>,
    missing_data_reason: Option<&str>,
) -> FrameCheckResult {
    evaluate_fixture_with_sources(
        fixture,
        fixture_path,
        repo_root,
        main_checkout,
        locator,
        missing_data_reason,
        None,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn evaluate_fixture_with_sources(
    fixture: &FrameFixture,
    fixture_path: &Path,
    repo_root: &Path,
    main_checkout: Option<&Path>,
    locator: Option<&MapLocator>,
    missing_data_reason: Option<&str>,
    predecoded_session_frame: Option<(&Path, &FrameBuf)>,
    session_skip_reason: Option<&str>,
    relocator: Option<&TeyvatGlobalRelocator>,
    relocator_skip_reason: Option<&str>,
) -> FrameCheckResult {
    let fixture_path_str = fixture_path.display().to_string();
    let mut resolved_image_str: Option<String> = None;
    let mut resolved_png_path: Option<PathBuf> = None;
    let mut session_buf: Option<&FrameBuf> = None;

    if let Some(ref img_rel) = fixture.image {
        let Some(path) = resolve_fixture_image(repo_root, main_checkout, img_rel) else {
            return build_skipped_result(
                fixture,
                fixture_path_str,
                None,
                format!("image not found: {img_rel}"),
            );
        };
        resolved_image_str = Some(path.display().to_string());
        resolved_png_path = Some(path);
    } else if let Some(sf) = &fixture.session_frame {
        if let Some(reason) = session_skip_reason {
            return build_skipped_result(fixture, fixture_path_str, None, reason.to_owned());
        }
        let Some((video_path, buf)) = predecoded_session_frame else {
            return build_skipped_result(
                fixture,
                fixture_path_str,
                None,
                format!(
                    "session frame {}:{} not available",
                    sf.session, sf.frame
                ),
            );
        };
        resolved_image_str = Some(format!("{}#f{}", video_path.display(), sf.frame));
        session_buf = Some(buf);
    }

    if fixture.mmap_locate.is_some() && locator.is_none() {
        return build_skipped_result(
            fixture,
            fixture_path_str,
            resolved_image_str,
            missing_data_reason
                .unwrap_or("required map datasets missing in data_dir")
                .to_owned(),
        );
    }

    if fixture.global_relocate.is_some() && relocator.is_none() {
        return build_skipped_result(
            fixture,
            fixture_path_str,
            resolved_image_str,
            relocator_skip_reason
                .unwrap_or("terrain_n2_v4.npz missing for global_relocate")
                .to_owned(),
        );
    }

    let resolved_prime_image = if let Some(hint) = fixture
        .mmap_locate
        .as_ref()
        .and_then(|m| m.prev_hint.as_ref())
    {
        match resolve_fixture_image(repo_root, main_checkout, &hint.prime_image) {
            Some(p) => Some(p),
            None => {
                return build_skipped_result(
                    fixture,
                    fixture_path_str,
                    resolved_image_str,
                    format!("prime_image not found: {}", hint.prime_image),
                );
            }
        }
    } else {
        None
    };

    let mut failures = Vec::new();
    let mut detail: Option<String> = None;

    // (a) Verify PNG dimensions match fixture width x height (image fixtures only)
    if let Some(ref resolved_image) = resolved_png_path {
        match image::image_dimensions(resolved_image) {
            Ok((img_w, img_h)) => {
                if img_w != fixture.width || img_h != fixture.height {
                    failures.push(format!(
                        "image dimensions mismatch: PNG is {img_w}x{img_h}, fixture expects {}x{}",
                        fixture.width, fixture.height
                    ));
                }
            }
            Err(err) => {
                failures.push(format!(
                    "failed to read PNG dimensions from '{}': {err}",
                    resolved_image.display()
                ));
            }
        }
    }

    // (b) Optional layout / HUD crop checks
    if fixture.has_layout_check() {
        let exp_mm = fixture.expected_minimap_rect.unwrap();
        let exp_pm = fixture.expected_paimon_rect.unwrap();
        let tol = fixture.tolerance_px.unwrap();
        let true_circle = fixture.true_minimap_circle.unwrap();
        match compute_ui_layout(fixture.width, fixture.height, fixture.is_gamepad) {
            Ok(layout) => {
                check_rect_within_tol(
                    "minimap_rect",
                    FixtureRect::from_rect_i32(layout.minimap_rect),
                    exp_mm,
                    tol,
                    &mut failures,
                );
                check_rect_within_tol(
                    "paimon_rect",
                    FixtureRect::from_rect_i32(layout.paimon_rect),
                    exp_pm,
                    tol,
                    &mut failures,
                );
                if fixture.expect_circle_in_crop {
                    let mm = layout.minimap_rect;
                    check_circle_inside_box(
                        "layout.minimap_rect",
                        true_circle,
                        mm.x as f64,
                        mm.y as f64,
                        (mm.x + mm.width) as f64,
                        (mm.y + mm.height) as f64,
                        &mut failures,
                    );

                    let (hud_w, hud_h) = compute_tracker_hud_crop(&layout);
                    check_circle_inside_box(
                        "tracker hud_crop",
                        true_circle,
                        0.0,
                        0.0,
                        hud_w as f64,
                        hud_h as f64,
                        &mut failures,
                    );
                }
            }
            Err(err) => {
                failures.push(format!("compute_ui_layout failed: {err}"));
            }
        }
    }

    let mut decoded_png_buf: Option<FrameBuf> = None;

    // (c) Optional M-Map locate checks (original + deterministic +/-2 LSB perturbations)
    if let (Some(exp), Some(loc)) = (&fixture.mmap_locate, locator) {
        let mut prime_decode_ok = true;
        let prime_frame = match (&exp.prev_hint, &resolved_prime_image) {
            (Some(_), Some(prime_path)) => match load_png_as_bgra(prime_path) {
                Ok(buf) => Some(buf),
                Err(err) => {
                    failures.push(format!("failed to decode prime_image for prev_hint: {err}"));
                    prime_decode_ok = false;
                    None
                }
            },
            _ => None,
        };

        let prepare_locator_state = |loc: &MapLocator, failures: &mut Vec<String>| -> bool {
            loc.reset_tracking_state();
            if let (Some(hint), Some(p_buf)) = (&exp.prev_hint, &prime_frame) {
                let _ = loc.locate_player_frame(p_buf.as_view(), false, None);
                let got_map = loc.last_map_id();
                let got_pos = loc.last_known_pos();
                let ok = match (got_map, got_pos) {
                    (Some(mid), Some((wx, wy))) => {
                        mid == hint.map_id && (wx - hint.world_x).hypot(wy - hint.world_y) <= 50.0
                    }
                    _ => false,
                };
                if !ok {
                    failures.push(format!(
                        "mmap_locate[prev_hint]: failed to prime state via '{}' (got map_id={got_map:?}, pos={got_pos:?}; exp map_id={} ({:.1}, {:.1}))",
                        hint.prime_image, hint.map_id, hint.world_x, hint.world_y
                    ));
                    return false;
                }
            }
            true
        };

        if prime_decode_ok {
            let orig_opt: Option<&FrameBuf> = if let Some(buf) = session_buf {
                Some(buf)
            } else if let Some(ref resolved_image) = resolved_png_path {
                match load_png_as_bgra(resolved_image) {
                    Ok(buf) => {
                        decoded_png_buf = Some(buf);
                        decoded_png_buf.as_ref()
                    }
                    Err(err) => {
                        failures.push(format!("failed to decode PNG for mmap_locate: {err}"));
                        None
                    }
                }
            } else {
                None
            };
            if let Some(orig_frame) = orig_opt {
                if prepare_locator_state(loc, &mut failures) {
                    let orig_res = loc.locate_player_frame(orig_frame.as_view(), false, None);
                    check_locate_result("orig", orig_res.as_ref(), exp, &mut failures);
                }

                for &seed in &exp.perturb_seeds {
                    let perturbed = perturb_bgra_frame(orig_frame, seed);
                    if prepare_locator_state(loc, &mut failures) {
                        let pert_res = loc.locate_player_frame(perturbed.as_view(), false, None);
                        check_locate_result(
                            &format!("perturb(seed={seed})"),
                            pert_res.as_ref(),
                            exp,
                            &mut failures,
                        );
                    }
                }
            }
        }
    }

    // (d) Optional TeyvatGlobalRelocator crop checks
    if let (Some(exp), Some(reloc)) = (&fixture.global_relocate, relocator) {
        let frame_opt: Option<&FrameBuf> = if let Some(buf) = session_buf {
            Some(buf)
        } else if let Some(ref buf) = decoded_png_buf {
            Some(buf)
        } else if let Some(ref resolved_image) = resolved_png_path {
            match load_png_as_bgra(resolved_image) {
                Ok(buf) => {
                    decoded_png_buf = Some(buf);
                    decoded_png_buf.as_ref()
                }
                Err(err) => {
                    failures.push(format!("failed to decode PNG for global_relocate: {err}"));
                    None
                }
            }
        } else {
            None
        };
        if let Some(frame) = frame_opt {
            detail = Some(check_global_relocate_result(reloc, frame, exp, &mut failures));
        }
    }

    let outcome = classify_outcome(failures.is_empty(), fixture.known_bug.is_some());

    FrameCheckResult {
        name: fixture.name.clone(),
        fixture_path: fixture_path_str,
        resolved_image: resolved_image_str,
        outcome,
        known_bug: fixture.known_bug.clone(),
        detail,
        failures,
        skip_reason: None,
    }
}

pub fn load_fixtures_from_dir(dir: &Path) -> Result<Vec<(PathBuf, FrameFixture)>, String> {
    if !dir.is_dir() {
        return Err(format!(
            "frame fixtures directory does not exist or is not a directory: {}",
            dir.display()
        ));
    }

    let mut json_paths = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to read fixtures directory '{}': {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            json_paths.push(path);
        }
    }
    json_paths.sort();

    let mut fixtures = Vec::with_capacity(json_paths.len());
    for path in json_paths {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read fixture '{}': {e}", path.display()))?;
        let fixture: FrameFixture = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid fixture JSON '{}': {e}", path.display()))?;
        fixture.validate(&path.display().to_string())?;
        fixtures.push((path, fixture));
    }
    Ok(fixtures)
}

pub fn run_frames(opts: &FramesOptions) -> Result<FramesReport, String> {
    let repo_root = match &opts.repo_root {
        Some(p) => p.clone(),
        None => std::env::current_dir()
            .map_err(|e| format!("failed to resolve current directory: {e}"))?,
    };
    let fixtures_dir = if opts.dir.is_absolute() {
        opts.dir.clone()
    } else {
        repo_root.join(&opts.dir)
    };
    let fixtures = load_fixtures_from_dir(&fixtures_dir)?;
    let main_checkout = resolve_main_checkout(&repo_root);

    let needs_locator = fixtures.iter().any(|(_, f)| f.mmap_locate.is_some());
    let (locator, missing_data_reason): (Option<MapLocator>, Option<String>) = if needs_locator {
        match resolve_data_dir(&repo_root, main_checkout.as_deref(), &opts.data_dir) {
            Some(resolved_data) => (Some(build_frame_locator(&resolved_data)), None),
            None => (
                None,
                Some(format!(
                    "required map datasets missing in data_dir '{}' (need {})",
                    opts.data_dir.display(),
                    gh_core::assets::REQUIRED_ASSETS.join(", ")
                )),
            ),
        }
    } else {
        (None, None)
    };

    let has_session_fixtures = fixtures.iter().any(|(_, f)| f.session_frame.is_some());
    let sessions_dir = if has_session_fixtures {
        resolve_sessions_dir(&repo_root, main_checkout.as_deref(), opts.sessions_dir.as_deref())
    } else {
        None
    };

    if has_session_fixtures
        && let Some(ref ff) = opts.ffmpeg_path
        && (!ff.is_absolute() || !ff.is_file())
    {
        return Err(format!(
            "--ffmpeg must be an existing absolute file path (never resolved from PATH): {}",
            ff.display()
        ));
    }

    // Batch-decode all requested frames grouped by (video_path, crop_rect) in one ffmpeg call per group.
    let mut batch_requests: BTreeMap<BatchCropKey, BTreeSet<usize>> = BTreeMap::new();
    let mut fixture_session_keys: Vec<Option<DecodedFrameKey>> = vec![None; fixtures.len()];
    let mut fixture_session_skips: Vec<Option<String>> = vec![None; fixtures.len()];

    for (idx, (_, fixture)) in fixtures.iter().enumerate() {
        let Some(sf) = &fixture.session_frame else {
            continue;
        };
        let Some(ref ff) = opts.ffmpeg_path else {
            fixture_session_skips[idx] =
                Some("ffmpeg path not provided (--ffmpeg) for session_frame".to_owned());
            continue;
        };
        let _ = ff;
        let Some(ref sdir) = sessions_dir else {
            fixture_session_skips[idx] = Some(format!(
                "sessions directory not found for '{}'",
                sf.session
            ));
            continue;
        };
        let video_path = sdir.join(&sf.session).join("video.mkv");
        if !video_path.is_file() {
            fixture_session_skips[idx] = Some(format!(
                "session video not found: {}",
                video_path.display()
            ));
            continue;
        }
        let crop_rect = if sf.crop_minimap {
            let lay = compute_ui_layout(fixture.width, fixture.height, fixture.is_gamepad)
                .map_err(|e| format!("compute_ui_layout failed: {e}"))?;
            lay.minimap_rect
        } else {
            RectI32 {
                x: 0,
                y: 0,
                width: fixture.width as i32,
                height: fixture.height as i32,
            }
        };
        let batch_key = BatchCropKey {
            video_path,
            x: crop_rect.x,
            y: crop_rect.y,
            w: crop_rect.width,
            h: crop_rect.height,
        };
        batch_requests
            .entry(batch_key.clone())
            .or_default()
            .insert(sf.frame);
        fixture_session_keys[idx] = Some(DecodedFrameKey {
            batch: batch_key,
            frame_1based: sf.frame,
        });
    }

    let mut decoded_session_frames: HashMap<DecodedFrameKey, FrameBuf> = HashMap::new();
    if let Some(ref ff) = opts.ffmpeg_path {
        for (batch, frames) in &batch_requests {
            let chunk = decode_video_crop_batch(ff, batch, frames)?;
            decoded_session_frames.extend(chunk);
        }
    }

    // Build and cache TeyvatGlobalRelocator once per distinct terrain_n2_v4.npz directory.
    let reloc_tmp = TempDirGuard::new("reloc_idx");
    let mut relocators: HashMap<PathBuf, Arc<TeyvatGlobalRelocator>> = HashMap::new();
    let fallback_terrain_dir = resolve_data_dir_with_check(
        &repo_root,
        main_checkout.as_deref(),
        &opts.data_dir,
        has_terrain_n2_dataset,
    );
    let mut fixture_relocators: Vec<Option<Arc<TeyvatGlobalRelocator>>> =
        vec![None; fixtures.len()];
    let mut fixture_relocator_skips: Vec<Option<String>> = vec![None; fixtures.len()];

    for (idx, (_, fixture)) in fixtures.iter().enumerate() {
        if fixture.global_relocate.is_none() || fixture_session_skips[idx].is_some() {
            continue;
        }
        let mut candidate_dir: Option<PathBuf> = None;
        if let (Some(sf), Some(sdir)) = (&fixture.session_frame, &sessions_dir) {
            let sess_data = sdir.join(&sf.session).join("assets_cache").join("data");
            if has_terrain_n2_dataset(&sess_data) {
                candidate_dir = Some(sess_data);
            }
        }
        let eff_dir = candidate_dir.or_else(|| fallback_terrain_dir.clone());
        let Some(data_dir) = eff_dir else {
            fixture_relocator_skips[idx] = Some(format!(
                "terrain_n2_v4.npz missing in data_dir '{}'",
                opts.data_dir.display()
            ));
            continue;
        };

        if let Some(existing) = relocators.get(&data_dir) {
            fixture_relocators[idx] = Some(Arc::clone(existing));
            continue;
        }
        let npz_path = data_dir.join("terrain_n2_v4.npz");
        let Some(raster) = load_map2_raster_from_npz(&npz_path) else {
            fixture_relocator_skips[idx] = Some(format!(
                "failed to load map_2_gray/map_2_meta from {}",
                npz_path.display()
            ));
            continue;
        };
        let idx_id = relocators.len();
        let idx_path = reloc_tmp.path.join(format!("idx_{idx_id}_{GLOBAL_INDEX_FILENAME}"));
        let reloc = Arc::new(TeyvatGlobalRelocator::new_with_fallback(
            Some(raster),
            Some(idx_path),
            false,
            false,
        ));
        if !reloc.is_ready() {
            fixture_relocator_skips[idx] = Some(format!(
                "failed to build global relocator index from {}",
                npz_path.display()
            ));
            continue;
        }
        relocators.insert(data_dir, Arc::clone(&reloc));
        fixture_relocators[idx] = Some(reloc);
    }

    let mut report = FramesReport {
        pass: 0,
        fail: 0,
        known_fail: 0,
        xpass: 0,
        skipped: 0,
        results: Vec::with_capacity(fixtures.len()),
    };

    for (idx, (path, fixture)) in fixtures.iter().enumerate() {
        let predecoded = fixture_session_keys[idx].as_ref().and_then(|k| {
            decoded_session_frames
                .get(k)
                .map(|buf| (k.batch.video_path.as_path(), buf))
        });
        let res = evaluate_fixture_with_sources(
            fixture,
            path,
            &repo_root,
            main_checkout.as_deref(),
            locator.as_ref(),
            missing_data_reason.as_deref(),
            predecoded,
            fixture_session_skips[idx].as_deref(),
            fixture_relocators[idx].as_deref(),
            fixture_relocator_skips[idx].as_deref(),
        );
        match res.outcome {
            FrameOutcome::Pass => {
                report.pass += 1;
                if let Some(ref det) = res.detail {
                    println!(
                        "[{}] {} ({}x{}) -- {det}",
                        res.outcome.tag(),
                        res.name,
                        fixture.width,
                        fixture.height
                    );
                } else {
                    println!(
                        "[{}] {} ({}x{})",
                        res.outcome.tag(),
                        res.name,
                        fixture.width,
                        fixture.height
                    );
                }
            }
            FrameOutcome::Fail => {
                report.fail += 1;
                println!(
                    "[{}] {} ({}x{}) -- {}",
                    res.outcome.tag(),
                    res.name,
                    fixture.width,
                    fixture.height,
                    res.failures.join(" | ")
                );
            }
            FrameOutcome::KnownFail => {
                report.known_fail += 1;
                println!(
                    "[{}] {} ({}x{}) -- bug: {} | {}",
                    res.outcome.tag(),
                    res.name,
                    fixture.width,
                    fixture.height,
                    res.known_bug.as_deref().unwrap_or(""),
                    res.failures.join(" | ")
                );
            }
            FrameOutcome::Xpass => {
                report.xpass += 1;
                println!(
                    "[{}] {} ({}x{}) -- known_bug unexpectedly passed: {}",
                    res.outcome.tag(),
                    res.name,
                    fixture.width,
                    fixture.height,
                    res.known_bug.as_deref().unwrap_or("")
                );
            }
            FrameOutcome::Skipped => {
                report.skipped += 1;
                println!(
                    "[{}] {} ({}x{}) -- {}",
                    res.outcome.tag(),
                    res.name,
                    fixture.width,
                    fixture.height,
                    res.skip_reason.as_deref().unwrap_or("missing image")
                );
            }
        }
        report.results.push(res);
    }

    println!("{}", report.summary_line());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_cv::locator::TerrainRaster;
    use image::{ImageBuffer, Rgb};

    fn write_solid_png(path: &Path, width: u32, height: u32) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(width, height);
        img.save(path).expect("save png");
    }

    fn write_fixture_json(dir: &Path, filename: &str, fixture: &FrameFixture) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create fixtures dir");
        let path = dir.join(filename);
        let text = serde_json::to_string_pretty(fixture).expect("serialize fixture");
        std::fs::write(&path, text).expect("write fixture json");
        path
    }

    fn base_1080p_fixture(name: &str, image_rel: &str) -> FrameFixture {
        FrameFixture {
            schema_version: FRAME_FIXTURE_SCHEMA_VERSION,
            name: name.to_owned(),
            description: "1080p control".to_owned(),
            notes: "synthetic unit test fixture".to_owned(),
            image: Some(image_rel.to_owned()),
            session_frame: None,
            width: 1920,
            height: 1080,
            is_gamepad: false,
            known_bug: None,
            expected_minimap_rect: Some(FixtureRect {
                x: 59,
                y: 15,
                width: 218,
                height: 218,
            }),
            expected_paimon_rect: Some(FixtureRect {
                x: 25,
                y: 12,
                width: 68,
                height: 77,
            }),
            tolerance_px: Some(2),
            true_minimap_circle: Some(FixtureCircle {
                cx: 168.0,
                cy: 124.0,
                radius: 107.0,
            }),
            expect_circle_in_crop: true,
            mmap_locate: None,
            global_relocate: None,
        }
    }

    fn base_mmap_fixture(name: &str, image_rel: &str) -> FrameFixture {
        FrameFixture {
            schema_version: FRAME_FIXTURE_SCHEMA_VERSION,
            name: name.to_owned(),
            description: "mmap locate unit test".to_owned(),
            notes: "synthetic mmap fixture".to_owned(),
            image: Some(image_rel.to_owned()),
            session_frame: None,
            width: 2560,
            height: 1440,
            is_gamepad: false,
            known_bug: None,
            expected_minimap_rect: None,
            expected_paimon_rect: None,
            tolerance_px: None,
            true_minimap_circle: None,
            expect_circle_in_crop: true,
            mmap_locate: Some(MMapLocateExpectation {
                expected_map_id: 40,
                expected_world_x: 5072.0,
                expected_world_y: -174.0,
                pos_tolerance_w: 50.0,
                prev_hint: None,
                perturb_seeds: vec![101, 202],
            }),
            global_relocate: None,
        }
    }

    fn mock_locate_result(
        has_world_coords: bool,
        map_id: Option<i32>,
        world_x: Option<f64>,
        world_y: Option<f64>,
        inliers_count: Option<usize>,
    ) -> LocateResult {
        LocateResult {
            detected: true,
            has_world_coords,
            map_id,
            map_name: None,
            area_id: None,
            area_name: None,
            world_x,
            world_y,
            lat: world_y,
            lng: world_x,
            pixel_x: 1280.0,
            pixel_y: 720.0,
            heading: 0.0,
            scale: Some(1.0),
            inliers_count,
            rmse: Some(1.0),
            is_underground: false,
            in_game_is_underground: Some(false),
            floor_idx: 0,
            total_floors: 1,
            active_floor_idx: 0,
            surface_idx: 0,
            floor_name: "G".to_owned(),
            floor_short: "G".to_owned(),
            hoyolab_floor_name: None,
            group_id: None,
            group_name: None,
            floor_id: None,
            has_layers: false,
            zoom_ratio_t: Some(0.5),
            candidates: None,
            inliers: None,
            slider_res: None,
            region_text: None,
            target_map_id: None,
            target_area_id: None,
            slot_types: None,
            reason: None,
            debug_included: false,
        }
    }

    #[test]
    fn outcome_pass_on_matching_1080p_fixture() {
        let tmp = TempDirGuard::new("pass");
        let img_rel = "scratch/test_1080p.png";
        write_solid_png(&tmp.path.join(img_rel), 1920, 1080);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let f = base_1080p_fixture("pass_case", img_rel);
        write_fixture_json(&fixtures_dir, "01_pass.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.pass, 1);
        assert_eq!(report.fail, 0);
        assert_eq!(report.known_fail, 0);
        assert_eq!(report.xpass, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(
            report.summary_line(),
            "Frames: pass 1 | fail 0 | known-fail 0 | xpass 0 | skipped 0"
        );
    }

    #[test]
    fn outcome_fail_when_rect_exceeds_tolerance() {
        let tmp = TempDirGuard::new("fail");
        let img_rel = "scratch/test_fail.png";
        write_solid_png(&tmp.path.join(img_rel), 1920, 1080);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let mut f = base_1080p_fixture("fail_case", img_rel);
        f.expected_paimon_rect.as_mut().unwrap().x = 97;
        write_fixture_json(&fixtures_dir, "01_fail.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.fail, 1);
        assert_eq!(report.results[0].outcome, FrameOutcome::Fail);
        assert!(
            report.results[0]
                .failures
                .iter()
                .any(|m| m.contains("paimon_rect mismatch"))
        );
    }

    #[test]
    fn outcome_known_fail_when_failing_fixture_has_known_bug() {
        let tmp = TempDirGuard::new("known_fail");
        let img_rel = "scratch/test_known_fail.png";
        write_solid_png(&tmp.path.join(img_rel), 1920, 1080);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let mut f = base_1080p_fixture("known_fail_case", img_rel);
        f.known_bug = Some("synthetic mismatch for known-fail classification".to_owned());
        f.expected_minimap_rect.as_mut().unwrap().x = 200;
        f.expected_paimon_rect.as_mut().unwrap().x = 166;
        write_fixture_json(&fixtures_dir, "01_known_fail.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.fail, 0);
        assert_eq!(report.known_fail, 1);
        assert_eq!(report.results[0].outcome, FrameOutcome::KnownFail);
    }

    #[test]
    fn outcome_xpass_when_known_bug_fixture_passes() {
        let tmp = TempDirGuard::new("xpass");
        let img_rel = "scratch/test_xpass.png";
        write_solid_png(&tmp.path.join(img_rel), 1920, 1080);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let mut f = base_1080p_fixture("xpass_case", img_rel);
        f.known_bug = Some("bug already fixed".to_owned());
        write_fixture_json(&fixtures_dir, "01_xpass.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.xpass, 1);
        assert_eq!(report.fail, 0);
        assert_eq!(report.results[0].outcome, FrameOutcome::Xpass);
    }

    #[test]
    fn outcome_skipped_when_image_is_missing() {
        let tmp = TempDirGuard::new("skipped");
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let f = base_1080p_fixture("missing_img_case", "scratch/does_not_exist.png");
        write_fixture_json(&fixtures_dir, "01_missing.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.skipped, 1);
        assert_eq!(report.pass, 0);
        assert_eq!(report.fail, 0);
        assert_eq!(report.results[0].outcome, FrameOutcome::Skipped);
        assert!(report.results[0].skip_reason.is_some());
    }

    #[test]
    fn outcome_fail_when_true_minimap_circle_is_clipped_by_layout_or_hud_crop() {
        let tmp = TempDirGuard::new("circle_clip");
        let img_rel = "scratch/test_circle_clip.png";
        write_solid_png(&tmp.path.join(img_rel), 1920, 1080);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let mut f = base_1080p_fixture("circle_clip_case", img_rel);
        // Rects still match 1080p layout, but circle extends past minimap_rect (277) and hud_crop (325)
        f.true_minimap_circle = Some(FixtureCircle {
            cx: 241.0,
            cy: 124.0,
            radius: 106.5,
        });
        write_fixture_json(&fixtures_dir, "01_circle_clip.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.fail, 1);
        assert_eq!(report.results[0].outcome, FrameOutcome::Fail);
        let joined = report.results[0].failures.join(" | ");
        assert!(
            joined.contains("layout.minimap_rect") && joined.contains("tracker hud_crop"),
            "expected both minimap_rect and hud_crop clip failures, got: {joined}"
        );
    }

    #[test]
    fn schema_rejects_unknown_fields() {
        let tmp = TempDirGuard::new("unknown_field");
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        std::fs::create_dir_all(&fixtures_dir).expect("create dir");
        let bad_json = r#"{
            "schema_version": 1,
            "name": "bad",
            "description": "bad",
            "notes": "bad",
            "image": "scratch/a.png",
            "width": 1920,
            "height": 1080,
            "expected_minimap_rect": { "x": 59, "y": 15, "width": 218, "height": 218 },
            "expected_paimon_rect": { "x": 25, "y": 12, "width": 68, "height": 77 },
            "tolerance_px": 2,
            "true_minimap_circle": { "cx": 168.0, "cy": 124.0, "radius": 107.0 },
            "unexpected_extra": true
        }"#;
        std::fs::write(fixtures_dir.join("bad.json"), bad_json).expect("write bad json");

        let err = load_fixtures_from_dir(&fixtures_dir).expect_err("unknown field must fail");
        assert!(err.contains("unknown field"));
    }

    #[test]
    fn schema_validates_mmap_locate_and_check_modes() {
        // Missing both layout and mmap_locate
        let mut neither = base_mmap_fixture("neither", "scratch/a.png");
        neither.mmap_locate = None;
        let err = neither.validate("neither.json").expect_err("must reject empty check mode");
        assert!(err.contains("at least one check mode"), "unexpected error: {err}");

        // Partial layout fields
        let mut partial = base_mmap_fixture("partial", "scratch/a.png");
        partial.tolerance_px = Some(4);
        let err = partial.validate("partial.json").expect_err("must reject partial layout fields");
        assert!(err.contains("must all be present or all omitted"), "unexpected error: {err}");

        // Non-positive pos_tolerance_w
        let mut bad_tol = base_mmap_fixture("bad_tol", "scratch/a.png");
        bad_tol.mmap_locate.as_mut().unwrap().pos_tolerance_w = 0.0;
        let err = bad_tol.validate("bad_tol.json").expect_err("must reject zero tolerance");
        assert!(err.contains("pos_tolerance_w"), "unexpected error: {err}");

        // Non-finite pos_tolerance_w
        let mut nan_tol = base_mmap_fixture("nan_tol", "scratch/a.png");
        nan_tol.mmap_locate.as_mut().unwrap().pos_tolerance_w = f64::NAN;
        let err = nan_tol.validate("nan_tol.json").expect_err("must reject NaN tolerance");
        assert!(err.contains("pos_tolerance_w"), "unexpected error: {err}");

        // Bad prev_hint (empty prime_image, traversal path, non-finite coords)
        let mut bad_prime = base_mmap_fixture("bad_prime", "scratch/a.png");
        bad_prime.mmap_locate.as_mut().unwrap().prev_hint = Some(PrevWorldHint {
            map_id: 40,
            world_x: 5072.0,
            world_y: -174.0,
            prime_image: "../outside.png".to_owned(),
        });
        let err = bad_prime
            .validate("bad_prime.json")
            .expect_err("must reject traversal prime_image");
        assert!(err.contains("prev_hint.prime_image"), "unexpected error: {err}");

        let mut bad_hint_xy = base_mmap_fixture("bad_hint_xy", "scratch/a.png");
        bad_hint_xy.mmap_locate.as_mut().unwrap().prev_hint = Some(PrevWorldHint {
            map_id: 40,
            world_x: f64::INFINITY,
            world_y: -174.0,
            prime_image: "scratch/prime.png".to_owned(),
        });
        let err = bad_hint_xy
            .validate("bad_hint_xy.json")
            .expect_err("must reject non-finite prev_hint coords");
        assert!(err.contains("prev_hint world_x/y"), "unexpected error: {err}");

        // Valid mmap-only fixture
        let valid = base_mmap_fixture("valid_mmap", "scratch/a.png");
        valid.validate("valid.json").expect("valid mmap fixture should pass");
    }

    #[test]
    fn perturb_bgra_frame_is_deterministic_and_bounded_within_2_lsb() {
        let mut src = FrameBuf::new_zeroed(16, 16, PixelFormat::Bgra8);
        for (i, px) in src.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            // Include boundary values 0, 1, 254, 255 to exercise saturation
            px[0] = (i % 256) as u8;
            px[1] = if i % 4 == 0 { 0 } else if i % 4 == 1 { 255 } else { 128 };
            px[2] = ((i * 7) % 256) as u8;
            px[3] = 200; // non-255 input alpha must become 255
        }

        let p1 = perturb_bgra_frame(&src, 101);
        let p2 = perturb_bgra_frame(&src, 101);
        let p3 = perturb_bgra_frame(&src, 202);

        assert_eq!(p1.data, p2.data, "same seed must produce bit-exact output");
        assert_ne!(p1.data, p3.data, "different seeds must produce distinct output");

        let mut any_nonzero_delta = false;
        for (sp, dp) in src
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .zip(p1.data.as_chunks::<4>().0.iter())
        {
            for ch in 0..3 {
                let delta = dp[ch] as i16 - sp[ch] as i16;
                assert!(
                    (-2..=2).contains(&delta),
                    "channel {ch} delta {delta} outside [-2, 2] (src={}, dst={})",
                    sp[ch],
                    dp[ch]
                );
                if delta != 0 {
                    any_nonzero_delta = true;
                }
            }
            assert_eq!(dp[3], 255, "alpha must be preserved at 255");
        }
        assert!(any_nonzero_delta, "perturbation must modify pixel values");
    }

    #[test]
    fn mmap_locate_outcome_classification_handles_pass_fail_known_fail_xpass() {
        let exp = MMapLocateExpectation {
            expected_map_id: 40,
            expected_world_x: 5072.0,
            expected_world_y: -174.0,
            pos_tolerance_w: 50.0,
            prev_hint: None,
            perturb_seeds: vec![101, 202],
        };

        // Pass (within tolerance & matching map_id)
        let mut f = Vec::new();
        let good = mock_locate_result(true, Some(40), Some(5080.0), Some(-170.0), Some(11));
        check_locate_result("orig", Some(&good), &exp, &mut f);
        assert!(f.is_empty(), "expected pass, got {f:?}");
        assert_eq!(classify_outcome(f.is_empty(), false), FrameOutcome::Pass);
        assert_eq!(classify_outcome(f.is_empty(), true), FrameOutcome::Xpass);

        // Fail on position drift -> KnownFail when known_bug is set
        let mut f = Vec::new();
        let bad_pos = mock_locate_result(true, Some(40), Some(4889.0), Some(-186.0), Some(3));
        check_locate_result("orig", Some(&bad_pos), &exp, &mut f);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("dist_w=") && f[0].contains("inliers=3"));
        assert_eq!(classify_outcome(f.is_empty(), false), FrameOutcome::Fail);
        assert_eq!(classify_outcome(f.is_empty(), true), FrameOutcome::KnownFail);

        // Fail on wrong map_id even if coords match
        let mut f = Vec::new();
        let bad_map = mock_locate_result(true, Some(2), Some(5072.0), Some(-174.0), Some(5));
        check_locate_result("perturb(seed=101)", Some(&bad_map), &exp, &mut f);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("perturb(seed=101)") && f[0].contains("map_id=Some(2)"));

        // Fail on has_world_coords = false and None
        let mut f = Vec::new();
        let no_world = mock_locate_result(false, None, None, None, None);
        check_locate_result("orig", Some(&no_world), &exp, &mut f);
        check_locate_result("perturb(seed=202)", None, &exp, &mut f);
        assert_eq!(f.len(), 2);
        assert!(f[0].contains("has_world_coords=false"));
        assert!(f[1].contains("returned None"));
    }

    #[test]
    fn outcome_skipped_when_mmap_data_dir_is_missing() {
        let tmp = TempDirGuard::new("missing_data");
        let img_rel = "scratch/test_mmap.png";
        write_solid_png(&tmp.path.join(img_rel), 2560, 1440);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let f = base_mmap_fixture("mmap_missing_data", img_rel);
        write_fixture_json(&fixtures_dir, "01_mmap.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: tmp.path.join("nonexistent_data"),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(report.skipped, 1);
        assert_eq!(report.pass, 0);
        assert_eq!(report.fail, 0);
        assert_eq!(report.results[0].outcome, FrameOutcome::Skipped);
        assert!(
            report.results[0]
                .skip_reason
                .as_deref()
                .unwrap_or("")
                .contains("datasets missing")
        );
    }

    #[test]
    fn schema_validates_session_frame_and_global_relocate() {
        let mut both_sources = base_mmap_fixture("both", "scratch/a.png");
        both_sources.session_frame = Some(SessionFrameSource {
            session: "sess_demo".to_owned(),
            frame: 1,
            crop_minimap: true,
        });
        assert!(both_sources.validate("both.json").is_err());

        let mut traversal = FrameFixture {
            schema_version: FRAME_FIXTURE_SCHEMA_VERSION,
            name: "trav".to_owned(),
            description: "bad".to_owned(),
            notes: "bad".to_owned(),
            image: None,
            session_frame: Some(SessionFrameSource {
                session: "../escape".to_owned(),
                frame: 1,
                crop_minimap: true,
            }),
            width: 2560,
            height: 1440,
            is_gamepad: false,
            known_bug: None,
            expected_minimap_rect: None,
            expected_paimon_rect: None,
            tolerance_px: None,
            true_minimap_circle: None,
            expect_circle_in_crop: true,
            mmap_locate: None,
            global_relocate: Some(GlobalRelocateExpectation {
                expected_world_x: None,
                expected_world_y: None,
                pos_tolerance_w: None,
                min_bin_score: 3.5,
                min_votes_near_target: 0,
                expect_no_match: true,
            }),
        };
        assert!(traversal.validate("trav.json").is_err());

        traversal.session_frame.as_mut().unwrap().session = "sess_demo".to_owned();
        traversal.session_frame.as_mut().unwrap().frame = 0;
        assert!(traversal.validate("zero_frame.json").is_err());

        traversal.session_frame.as_mut().unwrap().frame = 5;
        traversal.validate("ok_neg.json").expect("valid negative global_relocate");

        let mut missing_coords = traversal.clone();
        missing_coords.global_relocate.as_mut().unwrap().expect_no_match = false;
        assert!(missing_coords.validate("missing_coords.json").is_err());
    }

    #[test]
    fn global_relocate_check_handles_pass_fail_neg_and_skipped() {
        // Synthetic textured 128x128 raster centered at world (5426, 4402)
        let mut gray_vec = vec![0u8; 128 * 128];
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next_u64 = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        for y in 0..128 {
            for x in 0..128 {
                let xf = x as f64;
                let yf = y as f64;
                let base = 115.0
                    + 45.0 * (xf * 0.17).sin() * (yf * 0.13).cos()
                    + 35.0 * (xf * 0.29 - yf * 0.23).cos();
                gray_vec[y * 128 + x] = base.clamp(15.0, 235.0) as u8;
            }
        }
        for _ in 0..70 {
            let cx = (next_u64() % 96 + 16) as isize;
            let cy = (next_u64() % 96 + 16) as isize;
            let rad = (next_u64() % 5 + 3) as isize;
            let col = (next_u64() % 180 + 40) as u8;
            for dy in -rad..=rad {
                for dx in -rad..=rad {
                    if dx * dx + dy * dy <= rad * rad {
                        gray_vec[(cy + dy) as usize * 128 + (cx + dx) as usize] = col;
                    }
                }
            }
        }
        let full_gray = FrameBuf {
            width: 128,
            height: 128,
            stride: 128,
            format: PixelFormat::Gray8,
            data: gray_vec.clone(),
        };
        let gray = gh_cv::ops::color::frame_view_to_gray_array(full_gray.as_view())
            .expect("gray array");
        let raster = Arc::new(TerrainRaster {
            gray,
            meta: [7.0, 6.0, -2.0, 2000.0, 2000.0],
        });
        let tmp = TempDirGuard::new("gr_eval");
        let idx_path = tmp.path.join(GLOBAL_INDEX_FILENAME);
        let reloc = TeyvatGlobalRelocator::new(Some(raster), Some(idx_path), false);
        assert!(reloc.is_ready());

        // Build 90x90 BGR8 crop centered at pixel (64, 64) -> world (5426, 4402)
        let mut crop = FrameBuf::new_zeroed(90, 90, PixelFormat::Bgr8);
        for y in 0..90 {
            for x in 0..90 {
                let v = gray_vec[(y + 19) * 128 + (x + 19)];
                let off = (y * 90 + x) * 3;
                crop.data[off] = v;
                crop.data[off + 1] = v;
                crop.data[off + 2] = v;
            }
        }

        let exp_match = GlobalRelocateExpectation {
            expected_world_x: Some(5426.0),
            expected_world_y: Some(4402.0),
            pos_tolerance_w: Some(240.0),
            min_bin_score: 1.0,
            min_votes_near_target: 1,
            expect_no_match: false,
        };
        let mut failures = Vec::new();
        let det_pass = check_global_relocate_result(&reloc, &crop, &exp_match, &mut failures);
        assert!(failures.is_empty(), "expected pass, got {failures:?} ({det_pass})");

        let flat = FrameBuf::new_zeroed(90, 90, PixelFormat::Bgr8);
        let exp_neg = GlobalRelocateExpectation {
            expected_world_x: None,
            expected_world_y: None,
            pos_tolerance_w: None,
            min_bin_score: DEFAULT_MIN_BIN_SCORE,
            min_votes_near_target: 0,
            expect_no_match: true,
        };
        let mut neg_failures = Vec::new();
        let _ = check_global_relocate_result(&reloc, &flat, &exp_neg, &mut neg_failures);
        assert!(neg_failures.is_empty(), "flat negative failed: {neg_failures:?}");

        let mut fail_flat = Vec::new();
        let _ = check_global_relocate_result(&reloc, &flat, &exp_match, &mut fail_flat);
        assert!(!fail_flat.is_empty(), "flat frame must fail positive match");

        // session_frame fixture without --ffmpeg must report Skipped (never Pass)
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let sf_fix = FrameFixture {
            schema_version: FRAME_FIXTURE_SCHEMA_VERSION,
            name: "session_skipped".to_owned(),
            description: "skip without ffmpeg".to_owned(),
            notes: "unit test".to_owned(),
            image: None,
            session_frame: Some(SessionFrameSource {
                session: "sess_missing".to_owned(),
                frame: 1,
                crop_minimap: true,
            }),
            width: 2560,
            height: 1440,
            is_gamepad: false,
            known_bug: None,
            expected_minimap_rect: None,
            expected_paimon_rect: None,
            tolerance_px: None,
            true_minimap_circle: None,
            expect_circle_in_crop: true,
            mmap_locate: None,
            global_relocate: Some(exp_neg),
        };
        write_fixture_json(&fixtures_dir, "01_sf.json", &sf_fix);
        let skip_report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
            data_dir: tmp.path.join("nonexistent_data"),
            ..FramesOptions::default()
        })
        .expect("run_frames");
        assert_eq!(skip_report.skipped, 1);
        assert_eq!(skip_report.pass, 0);
        assert_eq!(skip_report.fail, 0);
    }
}
