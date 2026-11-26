//! Git-tracked human-verified ground truth schema (`tests/verified_gt.json`).

use gh_core::atomic_io::write_json_atomic;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const VERIFIED_GT_SCHEMA_VERSION: u32 = 1;

pub const DEFAULT_POLICY_NOTE: &str = "Human-verified ground truth for diagnostic bundles and recorded sessions. Agents MUST NOT modify existing entries or relax tolerances without explicit user approval.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagGtEntry {
    pub expect_rejected: bool,
    pub expected_map_id: Option<i32>,
    pub ref_xy: Option<[f64; 2]>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCheckpointGt {
    #[serde(default)]
    pub frame_idx: Option<usize>,
    #[serde(default)]
    pub ts: Option<f64>,
    pub map_id: Option<i32>,
    #[serde(default)]
    pub ref_xy: Option<[f64; 2]>,
    #[serde(default = "default_tol_w")]
    pub tol_w: f64,
    #[serde(default)]
    pub heading_deg: Option<f64>,
    #[serde(default)]
    pub tol_deg: Option<f64>,
    #[serde(default)]
    pub is_underground: Option<bool>,
    #[serde(default)]
    pub expect_rejected: bool,
    #[serde(default)]
    pub note: String,
}

fn default_tol_w() -> f64 {
    60.0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionGtEntry {
    #[serde(default)]
    pub checkpoints: Vec<SessionCheckpointGt>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifiedGtFile {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_policy_note")]
    pub policy: String,
    #[serde(default)]
    pub diagnostics: BTreeMap<String, DiagGtEntry>,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionGtEntry>,
}

fn default_schema_version() -> u32 {
    VERIFIED_GT_SCHEMA_VERSION
}

fn default_policy_note() -> String {
    DEFAULT_POLICY_NOTE.to_owned()
}

impl Default for VerifiedGtFile {
    fn default() -> Self {
        Self {
            schema_version: VERIFIED_GT_SCHEMA_VERSION,
            policy: DEFAULT_POLICY_NOTE.to_owned(),
            diagnostics: BTreeMap::new(),
            sessions: BTreeMap::new(),
        }
    }
}

impl VerifiedGtFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("invalid JSON in {}: {e}", path.display()))
    }

    pub fn load_or_default(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load(path)
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), String> {
        let val = serde_json::to_value(self)
            .map_err(|e| format!("failed to serialize verified_gt: {e}"))?;
        write_json_atomic(path, &val)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))
    }
}
