//! Scripted navigation scenario loading, execution, evaluation, and reporting for `gh-sim qa`.
//!
//! A scenario injects synthetic web inputs (e.g. `on_nearby_pins`) and hotkeys into an existing
//! recorded session via the shared [`crate::timeline::InputTimeline`] and production dispatch path,
//! then checks post-tick [`NavFrameSnapshot`] fields at target frames (with optional
//! `within_frames` windows).

use crate::replay::{ReplayOptions, ReplayScenarioHook, round3, run_replay_with_hook};
use crate::timeline::TimelineInputKind;
use gh_app::REPLAY_DISPATCHABLE_HOTKEYS;
use gh_app::session_recorder::{NavFrameSnapshot, RECORDED_WEB_CALLBACKS};
use gh_app::state::UiFrameSnapshot;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const SCENARIO_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_SCENARIOS_DIR: &str = "tests/scenarios";
pub const VALID_NAV_FIELDS: &[&str] = &["target_id", "locked_target_id", "user_locked_id", "state"];
pub const VALID_UI_FIELDS: &[&str] = &[
    "guide_open",
    "guide_zoomed",
    "nav_edit_mode",
    "nav_hud_enabled",
    "radar_enabled",
    "radar_active",
    "radar_edit_mode",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenarioFile {
    schema_version: u32,
    name: String,
    session: String,
    description: String,
    #[serde(default)]
    known_bug: Option<String>,
    #[serde(default)]
    inputs: Vec<RawScenarioInput>,
    expect: Vec<RawScenarioExpectation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenarioInput {
    frame: usize,
    #[serde(default)]
    hotkey: Option<String>,
    #[serde(default)]
    web_input: Option<RawScenarioWebInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenarioWebInput {
    callback: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenarioExpectation {
    frame: usize,
    #[serde(default)]
    nav: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    ui: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    within_frames: usize,
}

/// Expected subset of [`NavFrameSnapshot`] fields. Unlisted fields (`None`) are not checked;
/// `Some(None)` expects JSON `null` (empty), and `Some(Some(s))` expects exact string `s`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavExpectation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_target_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_locked_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl NavExpectation {
    pub fn matches(&self, snap: &NavFrameSnapshot) -> bool {
        if let Some(ref exp) = self.target_id
            && snap.target_id != *exp
        {
            return false;
        }
        if let Some(ref exp) = self.locked_target_id
            && snap.locked_target_id != *exp
        {
            return false;
        }
        if let Some(ref exp) = self.user_locked_id
            && snap.user_locked_id != *exp
        {
            return false;
        }
        if let Some(ref exp) = self.state
            && snap.state != *exp
        {
            return false;
        }
        true
    }

    pub fn format_expected(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref v) = self.target_id {
            parts.push(format!("target_id={}", fmt_opt_str(v.as_deref())));
        }
        if let Some(ref v) = self.locked_target_id {
            parts.push(format!("locked_target_id={}", fmt_opt_str(v.as_deref())));
        }
        if let Some(ref v) = self.user_locked_id {
            parts.push(format!("user_locked_id={}", fmt_opt_str(v.as_deref())));
        }
        if let Some(ref v) = self.state {
            parts.push(format!("state=\"{v}\""));
        }
        format!("{{{}}}", parts.join(", "))
    }

    pub fn format_actual_for_fields(&self, snap: &NavFrameSnapshot) -> String {
        let mut parts = Vec::new();
        if self.target_id.is_some() {
            parts.push(format!("target_id={}", fmt_opt_str(snap.target_id.as_deref())));
        }
        if self.locked_target_id.is_some() {
            parts.push(format!(
                "locked_target_id={}",
                fmt_opt_str(snap.locked_target_id.as_deref())
            ));
        }
        if self.user_locked_id.is_some() {
            parts.push(format!(
                "user_locked_id={}",
                fmt_opt_str(snap.user_locked_id.as_deref())
            ));
        }
        if self.state.is_some() {
            parts.push(format!("state=\"{}\"", snap.state));
        }
        format!("{{{}}}", parts.join(", "))
    }
}

fn fmt_opt_str(v: Option<&str>) -> String {
    match v {
        Some(s) => format!("\"{s}\""),
        None => "null".to_owned(),
    }
}

/// Expected subset of [`UiFrameSnapshot`] boolean fields. Unlisted fields (`None`) are not checked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiExpectation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_zoomed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nav_edit_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nav_hud_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radar_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radar_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radar_edit_mode: Option<bool>,
}

impl UiExpectation {
    pub fn matches(&self, snap: &UiFrameSnapshot) -> bool {
        if let Some(exp) = self.guide_open
            && snap.guide_open != exp
        {
            return false;
        }
        if let Some(exp) = self.guide_zoomed
            && snap.guide_zoomed != exp
        {
            return false;
        }
        if let Some(exp) = self.nav_edit_mode
            && snap.nav_edit_mode != exp
        {
            return false;
        }
        if let Some(exp) = self.nav_hud_enabled
            && snap.nav_hud_enabled != exp
        {
            return false;
        }
        if let Some(exp) = self.radar_enabled
            && snap.radar_enabled != exp
        {
            return false;
        }
        if let Some(exp) = self.radar_active
            && snap.radar_active != exp
        {
            return false;
        }
        if let Some(exp) = self.radar_edit_mode
            && snap.radar_edit_mode != exp
        {
            return false;
        }
        true
    }

    pub fn format_expected(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.guide_open {
            parts.push(format!("guide_open={v}"));
        }
        if let Some(v) = self.guide_zoomed {
            parts.push(format!("guide_zoomed={v}"));
        }
        if let Some(v) = self.nav_edit_mode {
            parts.push(format!("nav_edit_mode={v}"));
        }
        if let Some(v) = self.nav_hud_enabled {
            parts.push(format!("nav_hud_enabled={v}"));
        }
        if let Some(v) = self.radar_enabled {
            parts.push(format!("radar_enabled={v}"));
        }
        if let Some(v) = self.radar_active {
            parts.push(format!("radar_active={v}"));
        }
        if let Some(v) = self.radar_edit_mode {
            parts.push(format!("radar_edit_mode={v}"));
        }
        format!("{{{}}}", parts.join(", "))
    }

    pub fn format_actual_for_fields(&self, snap: &UiFrameSnapshot) -> String {
        let mut parts = Vec::new();
        if self.guide_open.is_some() {
            parts.push(format!("guide_open={}", snap.guide_open));
        }
        if self.guide_zoomed.is_some() {
            parts.push(format!("guide_zoomed={}", snap.guide_zoomed));
        }
        if self.nav_edit_mode.is_some() {
            parts.push(format!("nav_edit_mode={}", snap.nav_edit_mode));
        }
        if self.nav_hud_enabled.is_some() {
            parts.push(format!("nav_hud_enabled={}", snap.nav_hud_enabled));
        }
        if self.radar_enabled.is_some() {
            parts.push(format!("radar_enabled={}", snap.radar_enabled));
        }
        if self.radar_active.is_some() {
            parts.push(format!("radar_active={}", snap.radar_active));
        }
        if self.radar_edit_mode.is_some() {
            parts.push(format!("radar_edit_mode={}", snap.radar_edit_mode));
        }
        format!("{{{}}}", parts.join(", "))
    }
}

/// One validated scripted input bound to a 1-based frame index (`meta.jsonl` `i`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScenarioInput {
    pub frame: usize,
    pub kind: TimelineInputKind,
}

/// One validated navigation and/or UI expectation at `[frame, frame + within_frames]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScenarioExpectation {
    pub frame: usize,
    pub within_frames: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nav: Option<NavExpectation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiExpectation>,
}

impl ScenarioExpectation {
    fn format_expected(&self) -> String {
        match (&self.nav, &self.ui) {
            (Some(n), None) => n.format_expected(),
            (None, Some(u)) => u.format_expected(),
            (Some(n), Some(u)) => format!("nav={}, ui={}", n.format_expected(), u.format_expected()),
            (None, None) => "{}".to_owned(),
        }
    }

    fn format_actual(
        &self,
        nav_snap: Option<&NavFrameSnapshot>,
        ui_snap: Option<&UiFrameSnapshot>,
    ) -> String {
        match (&self.nav, &self.ui) {
            (Some(n), None) => nav_snap
                .map(|s| n.format_actual_for_fields(s))
                .unwrap_or_else(|| "<missing nav snapshot>".to_owned()),
            (None, Some(u)) => ui_snap
                .map(|s| u.format_actual_for_fields(s))
                .unwrap_or_else(|| "<missing ui snapshot>".to_owned()),
            (Some(n), Some(u)) => {
                let ns = nav_snap
                    .map(|s| n.format_actual_for_fields(s))
                    .unwrap_or_else(|| "<missing>".to_owned());
                let us = ui_snap
                    .map(|s| u.format_actual_for_fields(s))
                    .unwrap_or_else(|| "<missing>".to_owned());
                format!("nav={ns}, ui={us}")
            }
            (None, None) => "{}".to_owned(),
        }
    }
}

/// Validated scenario definition loaded from `tests/scenarios/*.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScenarioFile {
    pub schema_version: u32,
    pub name: String,
    pub session: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_bug: Option<String>,
    pub inputs: Vec<ScenarioInput>,
    pub expect: Vec<ScenarioExpectation>,
}

impl ScenarioFile {
    /// Highest 1-based frame index referenced by either `inputs` or `expect` (`frame + within_frames`).
    pub fn last_needed_frame(&self) -> usize {
        let max_in = self.inputs.iter().map(|i| i.frame).max().unwrap_or(0);
        let max_exp = self
            .expect
            .iter()
            .map(|e| e.frame.saturating_add(e.within_frames))
            .max()
            .unwrap_or(0);
        max_in.max(max_exp)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScenarioOutcome {
    Pass,
    Fail,
    KnownFail,
    Xpass,
}

impl ScenarioOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::KnownFail => "known-fail",
            Self::Xpass => "xpass",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioCheckResult {
    pub check_index: usize,
    pub frame: usize,
    pub within_frames: usize,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_frame: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<NavExpectation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_ui: Option<UiExpectation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_frame: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<NavFrameSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_ui: Option<UiFrameSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub name: String,
    pub session: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_bug: Option<String>,
    pub outcome: ScenarioOutcome,
    pub frames_replayed: usize,
    pub wall_elapsed_s: f64,
    pub checks_passed: usize,
    pub checks_total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<String>,
    pub checks: Vec<ScenarioCheckResult>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioSummary {
    pub pass: usize,
    pub fail: usize,
    pub known_fail: usize,
    pub xpass: usize,
}

impl ScenarioSummary {
    pub fn from_results(results: &[ScenarioResult]) -> Self {
        let mut s = Self::default();
        for r in results {
            match r.outcome {
                ScenarioOutcome::Pass => s.pass += 1,
                ScenarioOutcome::Fail => s.fail += 1,
                ScenarioOutcome::KnownFail => s.known_fail += 1,
                ScenarioOutcome::Xpass => s.xpass += 1,
            }
        }
        s
    }

    pub fn format_line(&self) -> String {
        format!(
            "Scenarios: pass {} | fail {} | known-fail {} | xpass {}",
            self.pass, self.fail, self.known_fail, self.xpass
        )
    }
}

fn parse_opt_string_field(field: &str, val: &Value) -> Result<Option<String>, String> {
    match val {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(format!("nav field '{field}' must be a string or null")),
    }
}

fn parse_nav_expectation(
    nav_map: &BTreeMap<String, Value>,
    ctx: &str,
) -> Result<NavExpectation, String> {
    if nav_map.is_empty() {
        return Err(format!("{ctx}: nav expectation must specify at least one field"));
    }
    let mut out = NavExpectation::default();
    for (k, v) in nav_map {
        match k.as_str() {
            "target_id" => {
                out.target_id = Some(
                    parse_opt_string_field(k, v).map_err(|e| format!("{ctx}: {e}"))?,
                );
            }
            "locked_target_id" => {
                out.locked_target_id = Some(
                    parse_opt_string_field(k, v).map_err(|e| format!("{ctx}: {e}"))?,
                );
            }
            "user_locked_id" => {
                out.user_locked_id = Some(
                    parse_opt_string_field(k, v).map_err(|e| format!("{ctx}: {e}"))?,
                );
            }
            "state" => match v {
                Value::String(s) => out.state = Some(s.clone()),
                _ => return Err(format!("{ctx}: nav field 'state' must be a string")),
            },
            other => {
                return Err(format!(
                    "{ctx}: unknown nav field '{other}' (valid fields: {})",
                    VALID_NAV_FIELDS.join(", ")
                ));
            }
        }
    }
    Ok(out)
}

fn parse_bool_field(field: &str, val: &Value) -> Result<bool, String> {
    match val {
        Value::Bool(b) => Ok(*b),
        _ => Err(format!("ui field '{field}' must be a boolean")),
    }
}

fn parse_ui_expectation(
    ui_map: &BTreeMap<String, Value>,
    ctx: &str,
) -> Result<UiExpectation, String> {
    if ui_map.is_empty() {
        return Err(format!("{ctx}: ui expectation must specify at least one field"));
    }
    let mut out = UiExpectation::default();
    for (k, v) in ui_map {
        match k.as_str() {
            "guide_open" => {
                out.guide_open = Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            "guide_zoomed" => {
                out.guide_zoomed = Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            "nav_edit_mode" => {
                out.nav_edit_mode = Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            "nav_hud_enabled" => {
                out.nav_hud_enabled =
                    Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            "radar_enabled" => {
                out.radar_enabled = Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            "radar_active" => {
                out.radar_active = Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            "radar_edit_mode" => {
                out.radar_edit_mode =
                    Some(parse_bool_field(k, v).map_err(|e| format!("{ctx}: {e}"))?);
            }
            other => {
                return Err(format!(
                    "{ctx}: unknown ui field '{other}' (valid fields: {})",
                    VALID_UI_FIELDS.join(", ")
                ));
            }
        }
    }
    Ok(out)
}

/// Parses and validates a scenario JSON string.
pub fn parse_scenario_str(raw: &str, source_label: &str) -> Result<ScenarioFile, String> {
    let parsed: RawScenarioFile = serde_json::from_str(raw)
        .map_err(|e| format!("failed to parse scenario {source_label}: {e}"))?;

    if parsed.schema_version != SCENARIO_SCHEMA_VERSION {
        return Err(format!(
            "scenario {source_label}: unsupported schema_version {} (expected {SCENARIO_SCHEMA_VERSION})",
            parsed.schema_version
        ));
    }
    let name = parsed.name.trim().to_owned();
    if name.is_empty() {
        return Err(format!("scenario {source_label}: 'name' must be non-empty"));
    }
    let session = parsed.session.trim().to_owned();
    if session.is_empty() {
        return Err(format!("scenario {source_label}: 'session' must be non-empty"));
    }
    let known_bug = match parsed.known_bug {
        Some(b) => {
            let trimmed = b.trim().to_owned();
            if trimmed.is_empty() {
                return Err(format!(
                    "scenario {source_label}: 'known_bug' must be non-empty when present"
                ));
            }
            Some(trimmed)
        }
        None => None,
    };

    let mut inputs = Vec::with_capacity(parsed.inputs.len());
    for (idx, inp) in parsed.inputs.into_iter().enumerate() {
        if inp.frame == 0 {
            return Err(format!(
                "scenario {source_label}: inputs[{idx}].frame must be >= 1"
            ));
        }
        let kind = match (inp.hotkey, inp.web_input) {
            (Some(action), None) => {
                if !gh_app::is_replay_dispatchable_hotkey(&action) {
                    return Err(format!(
                        "scenario {source_label}: inputs[{idx}] unknown or non-replayable hotkey action '{action}' (allowed: {})",
                        REPLAY_DISPATCHABLE_HOTKEYS.join(", ")
                    ));
                }
                TimelineInputKind::Hotkey { action }
            }
            (None, Some(wi)) => {
                if !RECORDED_WEB_CALLBACKS.contains(&wi.callback.as_str()) {
                    return Err(format!(
                        "scenario {source_label}: inputs[{idx}] unknown web_input callback '{}' (allowed: {})",
                        wi.callback,
                        RECORDED_WEB_CALLBACKS.join(", ")
                    ));
                }
                if !wi.payload.is_object() {
                    return Err(format!(
                        "scenario {source_label}: inputs[{idx}].web_input.payload must be a JSON object"
                    ));
                }
                TimelineInputKind::WebInput {
                    callback: wi.callback,
                    payload: wi.payload,
                }
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "scenario {source_label}: inputs[{idx}] cannot specify both 'hotkey' and 'web_input'"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "scenario {source_label}: inputs[{idx}] must specify either 'hotkey' or 'web_input'"
                ));
            }
        };
        inputs.push(ScenarioInput {
            frame: inp.frame,
            kind,
        });
    }

    if parsed.expect.is_empty() {
        return Err(format!(
            "scenario {source_label}: 'expect' must contain at least one check"
        ));
    }

    let mut expect = Vec::with_capacity(parsed.expect.len());
    for (idx, exp) in parsed.expect.into_iter().enumerate() {
        if exp.frame == 0 {
            return Err(format!(
                "scenario {source_label}: expect[{idx}].frame must be >= 1"
            ));
        }
        let ctx = format!("scenario {source_label} expect[{idx}]");
        if exp.nav.is_none() && exp.ui.is_none() {
            return Err(format!(
                "{ctx}: expectation must specify at least one of 'nav' or 'ui'"
            ));
        }
        let nav = match exp.nav {
            Some(ref m) => Some(parse_nav_expectation(m, &ctx)?),
            None => None,
        };
        let ui = match exp.ui {
            Some(ref m) => Some(parse_ui_expectation(m, &ctx)?),
            None => None,
        };
        expect.push(ScenarioExpectation {
            frame: exp.frame,
            within_frames: exp.within_frames,
            nav,
            ui,
        });
    }

    Ok(ScenarioFile {
        schema_version: parsed.schema_version,
        name,
        session,
        description: parsed.description,
        known_bug,
        inputs,
        expect,
    })
}

/// Loads and validates one scenario JSON file from disk.
pub fn load_scenario_file(path: &Path) -> Result<ScenarioFile, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read scenario {}: {e}", path.display()))?;
    let label = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("scenario.json");
    parse_scenario_str(&raw, label)
}

/// Loads all `*.json` scenario files from `dir` sorted deterministically by filename.
///
/// When `require_dir_exists` is `false` (default `tests/scenarios` not overridden on CLI) and
/// `dir` does not exist, returns an empty `Vec`.
pub fn load_scenarios_from_dir(
    dir: &Path,
    require_dir_exists: bool,
) -> Result<Vec<ScenarioFile>, String> {
    if !dir.is_dir() {
        if require_dir_exists {
            return Err(format!(
                "scenarios directory does not exist: {}",
                dir.display()
            ));
        }
        return Ok(Vec::new());
    }

    let mut json_files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to read scenarios dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("scenarios dir entry error: {e}"))?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            json_files.push(path);
        }
    }
    json_files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let mut out = Vec::with_capacity(json_files.len());
    for p in json_files {
        out.push(load_scenario_file(&p)?);
    }
    Ok(out)
}

/// Evaluates all expectations of a scenario against per-frame post-tick nav and UI snapshots.
pub fn evaluate_scenario(
    scenario: &ScenarioFile,
    nav_by_frame: &BTreeMap<usize, NavFrameSnapshot>,
    ui_by_frame: &BTreeMap<usize, UiFrameSnapshot>,
    frames_replayed: usize,
    wall_elapsed_s: f64,
) -> ScenarioResult {
    let mut checks = Vec::with_capacity(scenario.expect.len());
    let mut checks_passed = 0usize;
    let mut first_failure: Option<String> = None;

    for (check_index, exp) in scenario.expect.iter().enumerate() {
        let start = exp.frame;
        let end = exp.frame.saturating_add(exp.within_frames);
        let mut matched_frame = None;
        let mut last_in_window: Option<usize> = None;

        let candidate_frames: BTreeSet<usize> = nav_by_frame
            .range(start..=end)
            .map(|(&k, _)| k)
            .chain(ui_by_frame.range(start..=end).map(|(&k, _)| k))
            .collect();

        for fidx in candidate_frames {
            last_in_window = Some(fidx);
            let nav_ok = match exp.nav {
                Some(ref ne) => nav_by_frame.get(&fidx).is_some_and(|s| ne.matches(s)),
                None => true,
            };
            let ui_ok = match exp.ui {
                Some(ref ue) => ui_by_frame.get(&fidx).is_some_and(|s| ue.matches(s)),
                None => true,
            };
            if nav_ok && ui_ok {
                matched_frame = Some(fidx);
                break;
            }
        }

        if let Some(mf) = matched_frame {
            checks_passed += 1;
            checks.push(ScenarioCheckResult {
                check_index,
                frame: exp.frame,
                within_frames: exp.within_frames,
                passed: true,
                matched_frame: Some(mf),
                expected: exp.nav.clone(),
                expected_ui: exp.ui.clone(),
                actual_frame: Some(mf),
                actual: exp.nav.as_ref().and_then(|_| nav_by_frame.get(&mf).cloned()),
                actual_ui: exp.ui.as_ref().and_then(|_| ui_by_frame.get(&mf).cloned()),
                failure_detail: None,
            });
        } else {
            let window_label = if exp.within_frames == 0 {
                format!("frame {start}")
            } else {
                format!("frames {start}..={end}")
            };
            let (actual_frame, actual, actual_ui, detail) = match last_in_window {
                Some(af) => {
                    let nav_snap = nav_by_frame.get(&af);
                    let ui_snap = ui_by_frame.get(&af);
                    (
                        Some(af),
                        exp.nav.as_ref().and_then(|_| nav_snap.cloned()),
                        exp.ui.as_ref().and_then(|_| ui_snap.cloned()),
                        format!(
                            "{window_label}: expected {}, got at f={af} {}",
                            exp.format_expected(),
                            exp.format_actual(nav_snap, ui_snap)
                        ),
                    )
                }
                None => (
                    None,
                    None,
                    None,
                    format!(
                        "{window_label}: expected {}, got <no frame replayed in window>",
                        exp.format_expected()
                    ),
                ),
            };
            if first_failure.is_none() {
                first_failure = Some(detail.clone());
            }
            checks.push(ScenarioCheckResult {
                check_index,
                frame: exp.frame,
                within_frames: exp.within_frames,
                passed: false,
                matched_frame: None,
                expected: exp.nav.clone(),
                expected_ui: exp.ui.clone(),
                actual_frame,
                actual,
                actual_ui,
                failure_detail: Some(detail),
            });
        }
    }

    let checks_total = checks.len();
    let all_ok = checks_passed == checks_total;
    let outcome = match (scenario.known_bug.is_some(), all_ok) {
        (false, true) => ScenarioOutcome::Pass,
        (false, false) => ScenarioOutcome::Fail,
        (true, false) => ScenarioOutcome::KnownFail,
        (true, true) => ScenarioOutcome::Xpass,
    };

    ScenarioResult {
        name: scenario.name.clone(),
        session: scenario.session.clone(),
        description: scenario.description.clone(),
        known_bug: scenario.known_bug.clone(),
        outcome,
        frames_replayed,
        wall_elapsed_s: round3(wall_elapsed_s),
        checks_passed,
        checks_total,
        first_failure,
        checks,
    }
}

/// Runs one scenario in an isolated replay of its target session, stopping at `last_needed_frame`.
pub fn run_scenario(
    scenario: &ScenarioFile,
    session_dir: &Path,
    data_dir: &Path,
    gt_file: &Path,
    ffmpeg_path: Option<&Path>,
) -> Result<ScenarioResult, String> {
    let t0 = Instant::now();
    let hook = ReplayScenarioHook {
        extra_inputs: scenario
            .inputs
            .iter()
            .map(|i| (i.frame, i.kind.clone()))
            .collect(),
        stop_at_frame: Some(scenario.last_needed_frame()),
        capture_nav_snapshots: true,
        capture_ui_snapshots: true,
    };
    let opts = ReplayOptions {
        source_dir: session_dir.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        gt_file: gt_file.to_path_buf(),
        tiles_dir: None,
        ug_dir: None,
        paimon_cache_path: None,
        ffmpeg_path: ffmpeg_path.map(Path::to_path_buf),
        max_frames: None,
        drift_threshold_w: crate::replay::DEFAULT_DRIFT_THRESHOLD_W,
        verify_mmap_view: false,
        render_out: None,
        out_report: None,
    };
    let out = run_replay_with_hook(&opts, Some(&hook))?;
    let elapsed = t0.elapsed().as_secs_f64();
    Ok(evaluate_scenario(
        scenario,
        &out.nav_snapshots,
        &out.ui_snapshots,
        out.report.performance.frames_processed,
        elapsed,
    ))
}

/// Formats one concise stdout line for a scenario result.
pub fn format_scenario_line(r: &ScenarioResult) -> String {
    match r.outcome {
        ScenarioOutcome::Pass => format!(
            "[scenario] {} ({}): pass ({}/{} checks, {}f, {:.2}s)",
            r.name, r.session, r.checks_passed, r.checks_total, r.frames_replayed, r.wall_elapsed_s
        ),
        ScenarioOutcome::Fail => format!(
            "[scenario] {} ({}): fail ({}/{} checks, {}f, {:.2}s) | {}",
            r.name,
            r.session,
            r.checks_passed,
            r.checks_total,
            r.frames_replayed,
            r.wall_elapsed_s,
            r.first_failure.as_deref().unwrap_or("check failed")
        ),
        ScenarioOutcome::KnownFail => format!(
            "[scenario] {} ({}): known-fail [known_bug: {}] ({}/{} checks, {}f, {:.2}s) | {}",
            r.name,
            r.session,
            r.known_bug.as_deref().unwrap_or(""),
            r.checks_passed,
            r.checks_total,
            r.frames_replayed,
            r.wall_elapsed_s,
            r.first_failure.as_deref().unwrap_or("check failed")
        ),
        ScenarioOutcome::Xpass => format!(
            "[scenario] {} ({}): XPASS - known bug no longer reproduces, remove known_bug [known_bug: {}] ({}/{} checks, {}f, {:.2}s)",
            r.name,
            r.session,
            r.known_bug.as_deref().unwrap_or(""),
            r.checks_passed,
            r.checks_total,
            r.frames_replayed,
            r.wall_elapsed_s
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(
        target_id: Option<&str>,
        locked_target_id: Option<&str>,
        user_locked_id: Option<&str>,
        state: &str,
    ) -> NavFrameSnapshot {
        NavFrameSnapshot {
            target_id: target_id.map(str::to_owned),
            locked_target_id: locked_target_id.map(str::to_owned),
            user_locked_id: user_locked_id.map(str::to_owned),
            state: state.to_owned(),
        }
    }

    #[test]
    fn parse_valid_scenario_file() {
        let raw = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "nav_next_control",
            "session": "sess_demo",
            "description": "Press nav_next after pins arrive",
            "inputs": [
                {
                    "frame": 2,
                    "web_input": {
                        "callback": "on_nearby_pins",
                        "payload": {
                            "map_id": 2,
                            "pins": [
                                {"id": "demo:alpha", "lat": 10.0, "lng": 20.0, "name": "Pin Alpha"}
                            ]
                        }
                    }
                },
                { "frame": 4, "hotkey": "nav_next" }
            ],
            "expect": [
                { "frame": 4, "nav": { "target_id": "demo:alpha", "user_locked_id": null, "state": "tracking" } },
                { "frame": 5, "within_frames": 3, "nav": { "locked_target_id": null } }
            ]
        }))
        .unwrap();

        let sc = parse_scenario_str(&raw, "valid.json").unwrap();
        assert_eq!(sc.schema_version, 1);
        assert_eq!(sc.name, "nav_next_control");
        assert_eq!(sc.session, "sess_demo");
        assert_eq!(sc.known_bug, None);
        assert_eq!(sc.inputs.len(), 2);
        assert_eq!(sc.expect.len(), 2);
        assert_eq!(sc.expect[0].within_frames, 0);
        assert_eq!(sc.expect[1].within_frames, 3);
        assert_eq!(sc.last_needed_frame(), 8);
    }

    #[test]
    fn parse_rejects_unknown_hotkey_and_unknown_web_callback() {
        let bad_hotkey = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "bad_hk",
            "session": "sess_demo",
            "description": "invalid hotkey",
            "inputs": [
                { "frame": 2, "hotkey": "debug" }
            ],
            "expect": [
                { "frame": 2, "nav": { "state": "idle" } }
            ]
        }))
        .unwrap();
        let err = parse_scenario_str(&bad_hotkey, "bad_hk.json").unwrap_err();
        assert!(err.contains("debug"), "got: {err}");

        let unknown_hk = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "unknown_hk",
            "session": "sess_demo",
            "description": "unknown hotkey",
            "inputs": [
                { "frame": 2, "hotkey": "fly_to_celestia" }
            ],
            "expect": [
                { "frame": 2, "nav": { "state": "idle" } }
            ]
        }))
        .unwrap();
        let err = parse_scenario_str(&unknown_hk, "unknown_hk.json").unwrap_err();
        assert!(err.contains("fly_to_celestia"), "got: {err}");

        let bad_cb = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "bad_cb",
            "session": "sess_demo",
            "description": "unknown callback",
            "inputs": [
                {
                    "frame": 2,
                    "web_input": { "callback": "on_shutdown", "payload": {} }
                }
            ],
            "expect": [
                { "frame": 2, "nav": { "state": "idle" } }
            ]
        }))
        .unwrap();
        let err = parse_scenario_str(&bad_cb, "bad_cb.json").unwrap_err();
        assert!(err.contains("on_shutdown"), "got: {err}");
    }

    #[test]
    fn parse_rejects_bad_nav_field_and_bad_types() {
        let bad_field = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "bad_field",
            "session": "sess_demo",
            "description": "unknown nav field",
            "inputs": [],
            "expect": [
                { "frame": 2, "nav": { "target": "demo:alpha" } }
            ]
        }))
        .unwrap();
        let err = parse_scenario_str(&bad_field, "bad_field.json").unwrap_err();
        assert!(err.contains("unknown nav field 'target'"), "got: {err}");

        let bad_state_type = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "bad_state",
            "session": "sess_demo",
            "description": "null state",
            "inputs": [],
            "expect": [
                { "frame": 2, "nav": { "state": null } }
            ]
        }))
        .unwrap();
        let err = parse_scenario_str(&bad_state_type, "bad_state.json").unwrap_err();
        assert!(err.contains("'state' must be a string"), "got: {err}");

        let bad_target_type = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "bad_target",
            "session": "sess_demo",
            "description": "numeric target_id",
            "inputs": [],
            "expect": [
                { "frame": 2, "nav": { "target_id": 123 } }
            ]
        }))
        .unwrap();
        let err = parse_scenario_str(&bad_target_type, "bad_target.json").unwrap_err();
        assert!(err.contains("'target_id' must be a string or null"), "got: {err}");

        let unknown_top = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "extra_key",
            "session": "sess_demo",
            "description": "extra top-level key",
            "bogus": true,
            "inputs": [],
            "expect": [
                { "frame": 2, "nav": { "state": "idle" } }
            ]
        }))
        .unwrap();
        assert!(parse_scenario_str(&unknown_top, "extra_key.json").is_err());

        let empty_nav = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "empty_nav",
            "session": "sess_demo",
            "description": "empty nav object",
            "inputs": [],
            "expect": [
                { "frame": 2, "nav": {} }
            ]
        }))
        .unwrap();
        assert!(parse_scenario_str(&empty_nav, "empty_nav.json").is_err());
    }

    #[test]
    fn evaluate_within_frames_window_and_unlisted_fields() {
        let mut navs = BTreeMap::new();
        navs.insert(10, snap(Some("demo:beta"), None, Some("demo:beta"), "tracking"));
        navs.insert(11, snap(Some("demo:beta"), None, None, "tracking"));
        navs.insert(12, snap(Some("demo:alpha"), None, None, "arrived"));
        navs.insert(13, snap(Some("demo:alpha"), None, None, "arrived"));
        let uis = BTreeMap::new();

        let sc = ScenarioFile {
            schema_version: 1,
            name: "window_test".to_owned(),
            session: "sess_demo".to_owned(),
            description: "window matching".to_owned(),
            known_bug: None,
            inputs: vec![],
            expect: vec![
                // Matches on frame 12 (within [10, 12]); state is unlisted and ignored.
                ScenarioExpectation {
                    frame: 10,
                    within_frames: 2,
                    nav: Some(NavExpectation {
                        target_id: Some(Some("demo:alpha".to_owned())),
                        user_locked_id: Some(None),
                        ..Default::default()
                    }),
                    ui: None,
                },
            ],
        };
        let res = evaluate_scenario(&sc, &navs, &uis, 13, 0.1);
        assert_eq!(res.outcome, ScenarioOutcome::Pass);
        assert_eq!(res.checks[0].matched_frame, Some(12));

        // Narrow window [10, 11] misses frame 12 -> Fail with actual at f=11.
        let mut sc_tight = sc.clone();
        sc_tight.expect[0].within_frames = 1;
        let res_fail = evaluate_scenario(&sc_tight, &navs, &uis, 13, 0.1);
        assert_eq!(res_fail.outcome, ScenarioOutcome::Fail);
        assert_eq!(res_fail.checks[0].actual_frame, Some(11));
        let detail = res_fail.first_failure.as_deref().unwrap();
        assert!(detail.contains("frames 10..=11"), "got: {detail}");
        assert!(detail.contains("target_id=\"demo:alpha\""), "got: {detail}");
        assert!(detail.contains("target_id=\"demo:beta\""), "got: {detail}");
    }

    #[test]
    fn evaluate_all_four_outcome_kinds_and_summary_format() {
        let mut navs = BTreeMap::new();
        navs.insert(1, snap(Some("demo:alpha"), None, None, "tracking"));
        let uis = BTreeMap::new();

        let mk = |name: &str, known_bug: Option<&str>, exp_target: &str| ScenarioFile {
            schema_version: 1,
            name: name.to_owned(),
            session: "sess_demo".to_owned(),
            description: "outcome test".to_owned(),
            known_bug: known_bug.map(str::to_owned),
            inputs: vec![],
            expect: vec![ScenarioExpectation {
                frame: 1,
                within_frames: 0,
                nav: Some(NavExpectation {
                    target_id: Some(Some(exp_target.to_owned())),
                    ..Default::default()
                }),
                ui: None,
            }],
        };

        let r_pass = evaluate_scenario(&mk("ok", None, "demo:alpha"), &navs, &uis, 1, 0.01);
        let r_fail = evaluate_scenario(&mk("bad", None, "demo:beta"), &navs, &uis, 1, 0.01);
        let r_kfail =
            evaluate_scenario(&mk("bug_open", Some("bug #1"), "demo:beta"), &navs, &uis, 1, 0.01);
        let r_xpass =
            evaluate_scenario(&mk("bug_fixed", Some("bug #2"), "demo:alpha"), &navs, &uis, 1, 0.01);

        assert_eq!(r_pass.outcome, ScenarioOutcome::Pass);
        assert_eq!(r_fail.outcome, ScenarioOutcome::Fail);
        assert_eq!(r_kfail.outcome, ScenarioOutcome::KnownFail);
        assert_eq!(r_xpass.outcome, ScenarioOutcome::Xpass);

        let kfail_line = format_scenario_line(&r_kfail);
        assert!(kfail_line.contains("known-fail") && kfail_line.contains("bug #1"), "got: {kfail_line}");
        let xpass_line = format_scenario_line(&r_xpass);
        assert!(
            xpass_line.contains("known bug no longer reproduces, remove known_bug"),
            "got: {xpass_line}"
        );

        let summary = ScenarioSummary::from_results(&[r_pass, r_fail, r_kfail, r_xpass]);
        assert_eq!(
            summary.format_line(),
            "Scenarios: pass 1 | fail 1 | known-fail 1 | xpass 1"
        );
    }

    #[test]
    fn parse_and_validate_ui_expectations() {
        let valid_ui = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "ui_ok",
            "session": "sess_demo",
            "description": "ui and combined nav+ui expectations",
            "inputs": [
                { "frame": 4, "hotkey": "radar_toggle" }
            ],
            "expect": [
                {
                    "frame": 4,
                    "ui": {
                        "guide_open": false,
                        "guide_zoomed": false,
                        "nav_edit_mode": false,
                        "nav_hud_enabled": true,
                        "radar_enabled": true,
                        "radar_active": true,
                        "radar_edit_mode": false
                    }
                },
                {
                    "frame": 5,
                    "nav": { "state": "idle" },
                    "ui": { "radar_active": true }
                }
            ]
        }))
        .unwrap();
        let sc = parse_scenario_str(&valid_ui, "ui_ok.json").unwrap();
        assert_eq!(sc.expect.len(), 2);
        assert!(sc.expect[0].nav.is_none());
        let ui0 = sc.expect[0].ui.as_ref().unwrap();
        assert_eq!(ui0.guide_open, Some(false));
        assert_eq!(ui0.nav_hud_enabled, Some(true));
        assert_eq!(ui0.radar_enabled, Some(true));
        assert_eq!(ui0.radar_active, Some(true));
        assert!(sc.expect[1].nav.is_some() && sc.expect[1].ui.is_some());

        let missing_both = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "missing_both",
            "session": "sess_demo",
            "description": "neither nav nor ui",
            "inputs": [],
            "expect": [{ "frame": 2 }]
        }))
        .unwrap();
        let err = parse_scenario_str(&missing_both, "missing_both.json").unwrap_err();
        assert!(err.contains("at least one of 'nav' or 'ui'"), "got: {err}");

        let empty_ui = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "empty_ui",
            "session": "sess_demo",
            "description": "empty ui object",
            "inputs": [],
            "expect": [{ "frame": 2, "ui": {} }]
        }))
        .unwrap();
        let err = parse_scenario_str(&empty_ui, "empty_ui.json").unwrap_err();
        assert!(err.contains("ui expectation must specify at least one field"), "got: {err}");

        let unknown_ui = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "unknown_ui",
            "session": "sess_demo",
            "description": "unknown ui key",
            "inputs": [],
            "expect": [{ "frame": 2, "ui": { "radar_zoom": true } }]
        }))
        .unwrap();
        let err = parse_scenario_str(&unknown_ui, "unknown_ui.json").unwrap_err();
        assert!(err.contains("unknown ui field 'radar_zoom'"), "got: {err}");

        let non_bool_ui = serde_json::to_string(&json!({
            "schema_version": 1,
            "name": "non_bool_ui",
            "session": "sess_demo",
            "description": "string value for bool ui field",
            "inputs": [],
            "expect": [{ "frame": 2, "ui": { "guide_open": "true" } }]
        }))
        .unwrap();
        let err = parse_scenario_str(&non_bool_ui, "non_bool_ui.json").unwrap_err();
        assert!(err.contains("ui field 'guide_open' must be a boolean"), "got: {err}");
    }

    #[test]
    fn evaluate_ui_expectation_pass_and_fail_formatting() {
        let navs = BTreeMap::new();
        let mut uis = BTreeMap::new();
        uis.insert(
            4,
            UiFrameSnapshot {
                guide_open: false,
                guide_zoomed: false,
                nav_edit_mode: false,
                nav_hud_enabled: true,
                radar_enabled: false,
                radar_active: false,
                radar_edit_mode: false,
            },
        );
        uis.insert(
            6,
            UiFrameSnapshot {
                guide_open: true,
                guide_zoomed: false,
                nav_edit_mode: true,
                nav_hud_enabled: true,
                radar_enabled: true,
                radar_active: true,
                radar_edit_mode: true,
            },
        );

        let sc_ok = ScenarioFile {
            schema_version: 1,
            name: "ui_eval_ok".to_owned(),
            session: "sess_demo".to_owned(),
            description: "ui pass".to_owned(),
            known_bug: None,
            inputs: vec![],
            expect: vec![
                ScenarioExpectation {
                    frame: 4,
                    within_frames: 0,
                    nav: None,
                    ui: Some(UiExpectation {
                        radar_enabled: Some(false),
                        radar_active: Some(false),
                        ..Default::default()
                    }),
                },
                ScenarioExpectation {
                    frame: 6,
                    within_frames: 0,
                    nav: None,
                    ui: Some(UiExpectation {
                        guide_open: Some(true),
                        nav_edit_mode: Some(true),
                        radar_edit_mode: Some(true),
                        ..Default::default()
                    }),
                },
            ],
        };
        let res_ok = evaluate_scenario(&sc_ok, &navs, &uis, 6, 0.05);
        assert_eq!(res_ok.outcome, ScenarioOutcome::Pass);
        assert_eq!(res_ok.checks_passed, 2);
        assert_eq!(res_ok.checks[1].actual_ui, uis.get(&6).cloned());

        let sc_fail = ScenarioFile {
            schema_version: 1,
            name: "ui_eval_fail".to_owned(),
            session: "sess_demo".to_owned(),
            description: "ui fail".to_owned(),
            known_bug: None,
            inputs: vec![],
            expect: vec![ScenarioExpectation {
                frame: 4,
                within_frames: 0,
                nav: None,
                ui: Some(UiExpectation {
                    guide_open: Some(true),
                    ..Default::default()
                }),
            }],
        };
        let res_fail = evaluate_scenario(&sc_fail, &navs, &uis, 6, 0.05);
        assert_eq!(res_fail.outcome, ScenarioOutcome::Fail);
        let detail = res_fail.first_failure.as_deref().unwrap();
        assert!(
            detail.contains("frame 4: expected {guide_open=true}, got at f=4 {guide_open=false}"),
            "got: {detail}"
        );
    }

    #[test]
    fn tracked_scenarios_dir_parses_cleanly() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/scenarios");
        let scenarios = load_scenarios_from_dir(&dir, true).unwrap();
        assert!(
            scenarios.len() >= 2,
            "expected at least 2 tracked scenarios in tests/scenarios, found {}",
            scenarios.len()
        );
    }
}
