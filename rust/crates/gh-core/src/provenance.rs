//! Dataset fingerprinting and recording/replay provenance types shared between `gh-app` and `gh-sim`.

use crate::assets::{OPTIONAL_ASSETS, REQUIRED_ASSETS};
use crate::sha256::sha256_file;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Canonical dataset files tracked for live/replay parity (derived from `gh_core::assets`).
pub const DATASET_FILENAMES: [&str; 4] = [
    REQUIRED_ASSETS[0],
    REQUIRED_ASSETS[1],
    REQUIRED_ASSETS[2],
    OPTIONAL_ASSETS[0],
];

/// Per-file lowercase SHA-256 fingerprint of a map dataset directory (`None` / JSON `null` when missing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DatasetFingerprint(pub BTreeMap<String, Option<String>>);

impl Default for DatasetFingerprint {
    fn default() -> Self {
        let mut map = BTreeMap::new();
        for &name in &DATASET_FILENAMES {
            map.insert(name.to_owned(), None);
        }
        Self(map)
    }
}

impl DatasetFingerprint {
    /// Returns the recorded digest for `filename`, if present.
    pub fn get(&self, filename: &str) -> Option<&str> {
        self.0.get(filename).and_then(|v| v.as_deref())
    }

    /// Compares two fingerprints case-insensitively across all canonical dataset files (missing == missing).
    pub fn matches(&self, other: &Self) -> bool {
        DATASET_FILENAMES.iter().all(|&name| {
            opt_hash_eq(
                self.0.get(name).and_then(|v| v.as_deref()),
                other.0.get(name).and_then(|v| v.as_deref()),
            )
        })
    }
}

fn opt_hash_eq(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.trim().eq_ignore_ascii_case(y.trim()),
        (None, None) => true,
        _ => false,
    }
}

fn hash_optional_file(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    sha256_file(path).ok()
}

/// Computes the SHA-256 fingerprint for the four canonical dataset files in `data_dir`.
/// Missing or unreadable files are reported as `None` (serialized as JSON `null`).
pub fn fingerprint_data_dir(data_dir: &Path) -> DatasetFingerprint {
    let mut map = BTreeMap::new();
    for &name in &DATASET_FILENAMES {
        map.insert(name.to_owned(), hash_optional_file(&data_dir.join(name)));
    }
    DatasetFingerprint(map)
}

/// Provenance stamped into `<session>/info.json` by the live session recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProvenance {
    pub app_version: String,
    pub git_commit: String,
    /// Non-path source label (e.g. `"installed"`, `"dev"`, `"bundle"`, `"env_override"`).
    /// Must NEVER contain an absolute path or username.
    pub data_source: String,
    pub datasets: DatasetFingerprint,
}

/// Provenance section attached to `gh-sim` replay reports (and carried into `gh-sim qa`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayProvenance {
    pub recorded: Option<SessionProvenance>,
    /// `"session"` when loaded from `<session>/assets_cache/data/`, else `"data_dir"`.
    pub replay_data_source: String,
    pub replay_datasets: DatasetFingerprint,
    pub datasets_match: Option<bool>,
    pub same_commit: Option<bool>,
}

/// Ensures `data_source` is a short non-path label (never leaks filesystem paths or usernames).
pub fn sanitize_data_source_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
    {
        return "unknown".to_owned();
    }
    trimmed.to_owned()
}

/// Compares a recorded git commit SHA against the current build's commit SHA.
/// Returns `None` if either commit is empty or `"unknown"`.
pub fn compare_git_commits(recorded: &str, current: &str) -> Option<bool> {
    let r = recorded.trim();
    let c = current.trim();
    if r.is_empty()
        || c.is_empty()
        || r.eq_ignore_ascii_case("unknown")
        || c.eq_ignore_ascii_case("unknown")
    {
        return None;
    }
    Some(r.eq_ignore_ascii_case(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::sha256_hex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_core_prov_{label}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fingerprint_data_dir_hashes_present_files_and_marks_missing_null() {
        let dir = temp_dir("fp");
        let mm_bytes = br#"{"version":"4.0.0","maps":{}}"#;
        let terrain_bytes = b"PK\x03\x04terrain";
        std::fs::write(dir.join(DATASET_FILENAMES[0]), mm_bytes).unwrap();
        std::fs::write(dir.join(DATASET_FILENAMES[1]), terrain_bytes).unwrap();
        // DATASET_FILENAMES[2] and DATASET_FILENAMES[3] intentionally omitted

        let fp = fingerprint_data_dir(&dir);
        assert_eq!(fp.get(DATASET_FILENAMES[0]), Some(sha256_hex(mm_bytes).as_str()));
        assert_eq!(
            fp.get(DATASET_FILENAMES[1]),
            Some(sha256_hex(terrain_bytes).as_str())
        );
        assert_eq!(fp.get(DATASET_FILENAMES[2]), None);
        assert_eq!(fp.get(DATASET_FILENAMES[3]), None);

        // Verify lowercase hex & exact JSON key names with null for missing files
        let val = serde_json::to_value(&fp).unwrap();
        let obj = val.as_object().unwrap();
        assert_eq!(obj.len(), DATASET_FILENAMES.len());
        let mm_hex = obj[DATASET_FILENAMES[0]].as_str().unwrap();
        assert_eq!(mm_hex.len(), 64);
        assert_eq!(mm_hex, mm_hex.to_ascii_lowercase());
        assert!(obj[DATASET_FILENAMES[1]].is_string());
        assert!(obj[DATASET_FILENAMES[2]].is_null());
        assert!(obj[DATASET_FILENAMES[3]].is_null());

        // Missing directory produces all-None fingerprint without failing
        let missing_fp = fingerprint_data_dir(&dir.join("does_not_exist"));
        assert_eq!(missing_fp, DatasetFingerprint::default());
        assert!(!fp.matches(&missing_fp));
        assert!(fp.matches(&fp));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_data_source_label_rejects_paths_and_preserves_labels() {
        assert_eq!(sanitize_data_source_label("installed"), "installed");
        assert_eq!(sanitize_data_source_label(" dev "), "dev");
        let temp_abs = std::env::temp_dir().join("data");
        assert_eq!(
            sanitize_data_source_label(&temp_abs.to_string_lossy()),
            "unknown"
        );
        let posix_like = ["", "var", "data"].join("/");
        assert_eq!(sanitize_data_source_label(&posix_like), "unknown");
        assert_eq!(sanitize_data_source_label("dir\\sub"), "unknown");
        assert_eq!(sanitize_data_source_label(""), "unknown");
    }

    #[test]
    fn compare_git_commits_handles_equal_diff_and_unknown() {
        assert_eq!(compare_git_commits("aabbcc", "AABBCC"), Some(true));
        assert_eq!(compare_git_commits("aabbcc", "ddeeff"), Some(false));
        assert_eq!(compare_git_commits("unknown", "aabbcc"), None);
        assert_eq!(compare_git_commits("aabbcc", "unknown"), None);
        assert_eq!(compare_git_commits("", "aabbcc"), None);
    }
}
