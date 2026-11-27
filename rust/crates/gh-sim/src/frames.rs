//! Data-driven still-frame layout/crop regression fixtures (`gh-sim frames`).
//!
//! Validates production [`gh_core::ui_layout::compute_ui_layout`] and the tracker
//! HUD crop formula against real user-reported screenshots stored outside git
//! (`scratch/`). Missing images are reported as `skipped` (never `pass`).

use gh_core::frame::RectI32;
use gh_core::ui_layout::compute_ui_layout;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const FRAME_FIXTURE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_FRAME_FIXTURES_DIR: &str = "tests/frame_fixtures";

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

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameFixture {
    pub schema_version: u32,
    pub name: String,
    pub description: String,
    pub notes: String,
    pub image: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub is_gamepad: bool,
    #[serde(default)]
    pub known_bug: Option<String>,
    pub expected_minimap_rect: FixtureRect,
    pub expected_paimon_rect: FixtureRect,
    pub tolerance_px: i32,
    pub true_minimap_circle: FixtureCircle,
    #[serde(default = "default_true")]
    pub expect_circle_in_crop: bool,
}

impl FrameFixture {
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
        if self.image.trim().is_empty() {
            return Err(format!("{source_label}: image must be non-empty"));
        }
        let img_path = Path::new(&self.image);
        if img_path.is_absolute()
            || img_path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(format!(
                "{source_label}: image must be a safe repo-relative path, got '{}'",
                self.image
            ));
        }
        if self.width == 0 || self.height == 0 {
            return Err(format!(
                "{source_label}: invalid fixture dimensions {}x{}",
                self.width, self.height
            ));
        }
        if self.tolerance_px < 0 {
            return Err(format!(
                "{source_label}: tolerance_px must be >= 0, got {}",
                self.tolerance_px
            ));
        }
        if self.expected_minimap_rect.width <= 0 || self.expected_minimap_rect.height <= 0 {
            return Err(format!(
                "{source_label}: expected_minimap_rect must have positive size, got {}",
                self.expected_minimap_rect
            ));
        }
        if self.expected_paimon_rect.width <= 0 || self.expected_paimon_rect.height <= 0 {
            return Err(format!(
                "{source_label}: expected_paimon_rect must have positive size, got {}",
                self.expected_paimon_rect
            ));
        }
        let c = self.true_minimap_circle;
        if !c.cx.is_finite() || !c.cy.is_finite() || !c.radius.is_finite() || c.radius <= 0.0 {
            return Err(format!(
                "{source_label}: true_minimap_circle must be finite with radius > 0, got ({}, {}, r={})",
                c.cx, c.cy, c.radius
            ));
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
}

impl Default for FramesOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::from(DEFAULT_FRAME_FIXTURES_DIR),
            repo_root: None,
        }
    }
}

/// Computes the tracker HUD capture crop `(hud_w, hud_h)` matching
/// `gh-app/src/tracking_loop.rs` (`run_tick_worker`).
pub fn compute_tracker_hud_crop(
    frame_w: u32,
    frame_h: u32,
    minimap_rect: RectI32,
    paimon_maybe_rect: RectI32,
    scale_ui: f64,
) -> (i32, i32) {
    let cw = frame_w as i32;
    let ch = frame_h as i32;
    let cal_pad_x = (48.0 * scale_ui).round() as i32;
    let cal_pad_y = (28.0 * scale_ui).round() as i32;
    let hud_w = cw.min((minimap_rect.x + minimap_rect.width + cal_pad_x).max(paimon_maybe_rect.x + paimon_maybe_rect.width));
    let hud_h = ch.min((minimap_rect.y + minimap_rect.height + cal_pad_y).max(paimon_maybe_rect.y + paimon_maybe_rect.height));
    (hud_w, hud_h)
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

pub fn evaluate_fixture(
    fixture: &FrameFixture,
    fixture_path: &Path,
    repo_root: &Path,
    main_checkout: Option<&Path>,
) -> FrameCheckResult {
    let fixture_path_str = fixture_path.display().to_string();
    let Some(resolved_image) = resolve_fixture_image(repo_root, main_checkout, &fixture.image)
    else {
        return FrameCheckResult {
            name: fixture.name.clone(),
            fixture_path: fixture_path_str,
            resolved_image: None,
            outcome: FrameOutcome::Skipped,
            known_bug: fixture.known_bug.clone(),
            failures: Vec::new(),
            skip_reason: Some(format!("image not found: {}", fixture.image)),
        };
    };

    let mut failures = Vec::new();

    // (a) Verify PNG dimensions match fixture width x height
    match image::image_dimensions(&resolved_image) {
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

    // (b) Call production compute_ui_layout
    match compute_ui_layout(fixture.width, fixture.height, fixture.is_gamepad) {
        Ok(layout) => {
            // (c) minimap_rect vs expected_minimap_rect
            check_rect_within_tol(
                "minimap_rect",
                FixtureRect::from_rect_i32(layout.minimap_rect),
                fixture.expected_minimap_rect,
                fixture.tolerance_px,
                &mut failures,
            );

            // (d) paimon_rect vs expected_paimon_rect
            check_rect_within_tol(
                "paimon_rect",
                FixtureRect::from_rect_i32(layout.paimon_rect),
                fixture.expected_paimon_rect,
                fixture.tolerance_px,
                &mut failures,
            );

            // (e) true_minimap_circle inside layout.minimap_rect AND tracker hud_crop
            if fixture.expect_circle_in_crop {
                let mm = layout.minimap_rect;
                check_circle_inside_box(
                    "layout.minimap_rect",
                    fixture.true_minimap_circle,
                    mm.x as f64,
                    mm.y as f64,
                    (mm.x + mm.width) as f64,
                    (mm.y + mm.height) as f64,
                    &mut failures,
                );

                let (hud_w, hud_h) = compute_tracker_hud_crop(
                    fixture.width,
                    fixture.height,
                    layout.minimap_rect,
                    layout.paimon_maybe_rect,
                    layout.scale_ui,
                );
                check_circle_inside_box(
                    "tracker hud_crop",
                    fixture.true_minimap_circle,
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

    let all_passed = failures.is_empty();
    let outcome = match (all_passed, fixture.known_bug.is_some()) {
        (true, false) => FrameOutcome::Pass,
        (false, false) => FrameOutcome::Fail,
        (false, true) => FrameOutcome::KnownFail,
        (true, true) => FrameOutcome::Xpass,
    };

    FrameCheckResult {
        name: fixture.name.clone(),
        fixture_path: fixture_path_str,
        resolved_image: Some(resolved_image.display().to_string()),
        outcome,
        known_bug: fixture.known_bug.clone(),
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

    let mut report = FramesReport {
        pass: 0,
        fail: 0,
        known_fail: 0,
        xpass: 0,
        skipped: 0,
        results: Vec::with_capacity(fixtures.len()),
    };

    for (path, fixture) in &fixtures {
        let res = evaluate_fixture(fixture, path, &repo_root, main_checkout.as_deref());
        match res.outcome {
            FrameOutcome::Pass => {
                report.pass += 1;
                println!(
                    "[{}] {} ({}x{})",
                    res.outcome.tag(),
                    res.name,
                    fixture.width,
                    fixture.height
                );
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
    use image::{ImageBuffer, Rgb};
    use std::sync::atomic::{AtomicU64, Ordering};

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
            image: image_rel.to_owned(),
            width: 1920,
            height: 1080,
            is_gamepad: false,
            known_bug: None,
            expected_minimap_rect: FixtureRect {
                x: 59,
                y: 15,
                width: 218,
                height: 218,
            },
            expected_paimon_rect: FixtureRect {
                x: 25,
                y: 12,
                width: 68,
                height: 77,
            },
            tolerance_px: 2,
            true_minimap_circle: FixtureCircle {
                cx: 168.0,
                cy: 124.0,
                radius: 107.0,
            },
            expect_circle_in_crop: true,
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
        f.expected_paimon_rect.x = 97;
        write_fixture_json(&fixtures_dir, "01_fail.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
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
        let img_rel = "scratch/test_21x9.png";
        write_solid_png(&tmp.path.join(img_rel), 2560, 1080);
        let fixtures_dir = tmp.path.join("tests/frame_fixtures");
        let mut f = base_1080p_fixture("known_fail_case", img_rel);
        f.width = 2560;
        f.known_bug = Some("21:9 HUD inward shift".to_owned());
        f.expected_minimap_rect.x = 131;
        f.expected_paimon_rect.x = 97;
        f.true_minimap_circle = FixtureCircle {
            cx: 241.0,
            cy: 124.0,
            radius: 106.5,
        };
        write_fixture_json(&fixtures_dir, "01_known_fail.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
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
        f.true_minimap_circle = FixtureCircle {
            cx: 241.0,
            cy: 124.0,
            radius: 106.5,
        };
        write_fixture_json(&fixtures_dir, "01_circle_clip.json", &f);

        let report = run_frames(&FramesOptions {
            dir: fixtures_dir,
            repo_root: Some(tmp.path.clone()),
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
}
