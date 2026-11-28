//! Input-keyed disk cache for passing `gh-sim qa` session & scenario results.
//!
//! Safety-first design:
//! - Keys every entry on a SHA-256 of ALL real inputs (`gh-sim` binary bytes, `ffmpeg` binary,
//!   OCR language tag, datasets, `verified_gt.json`, `qa_baseline.json`, thresholds/overrides,
//!   commit/engine-unchanged provenance, session metadata/media/assets_cache, and scenario JSON).
//! - Any read, parse, or metadata ambiguity falls back cleanly to a cache miss (rebuilds live).
//! - Persists only passing session results and `Pass` / `KnownFail` scenario results so failures
//!   and fixed bugs (`Xpass`) always re-run on the next invocation.

use crate::qa::{QaSessionResult, QaThresholdOverrides, QaThresholds};
use crate::replay::{
    build_replay_provenance_with_repo, read_session_provenance, resolve_replay_data_dir,
};
use crate::scenario::{ScenarioFile, ScenarioOutcome, ScenarioResult};
use gh_app::BUILD_GIT_COMMIT;
use gh_app::session_recorder::WEB_INPUTS_FILENAME;
use gh_core::atomic_io::write_json_atomic;
use gh_core::provenance::DATASET_FILENAMES;
use gh_core::sha256::{sha256_file, sha256_hex};
use gh_win32::win_ocr::WindowsOcrService;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const QA_CACHE_SCHEMA_VERSION: u32 = 1;
pub const QA_CACHE_DIR_NAME: &str = "gh_sim_qa_cache_v1";
const VIDEO_SAMPLE_BYTES: usize = 64 * 1024;

pub fn default_qa_cache_dir() -> PathBuf {
    std::env::temp_dir().join(QA_CACHE_DIR_NAME)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaRunEnvFingerprint {
    pub exe_sha256: String,
    pub ffmpeg_sha256: Option<String>,
    pub ocr_lang: String,
    pub gt_sha256: Option<String>,
    pub baseline_sha256: Option<String>,
    pub effective_thresholds: QaThresholds,
    pub overrides: QaThresholdOverrides,
    pub current_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetCacheFingerprint {
    pub data_source: String,
    pub files: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaFingerprint {
    Video {
        filename: String,
        len: u64,
        mtime_ns: u128,
        head_sha256: String,
        mid_sha256: String,
        tail_sha256: String,
    },
    PngFrames {
        files: Vec<(String, u64, u128)>,
    },
    DiagnosticBundle {
        img_sha256: String,
        report_sha256: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInputFingerprint {
    pub session_name: String,
    pub dataset: DatasetCacheFingerprint,
    pub meta_sha256: String,
    pub info_sha256: Option<String>,
    pub web_inputs_sha256: Option<String>,
    pub blessed_poses_sha256: Option<String>,
    pub paimon_tpl_sha256: Option<String>,
    pub media: MediaFingerprint,
    pub assets_cache_files: Vec<(String, u64, u128)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSessionEntry {
    schema_version: u32,
    key: String,
    result: QaSessionResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedScenarioEntry {
    schema_version: u32,
    key: String,
    result: ScenarioResult,
}

fn file_mtime_ns(meta: &std::fs::Metadata) -> Option<u128> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

fn hash_required_file(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    sha256_file(path).ok()
}

fn hash_optional_present_file(path: &Path) -> Option<Option<String>> {
    if !path.exists() {
        return Some(None);
    }
    if !path.is_file() {
        return None;
    }
    sha256_file(path).ok().map(Some)
}

fn read_chunk_sha256(file: &mut File, offset: u64, len: usize) -> Option<String> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; len];
    let mut read_total = 0usize;
    while read_total < len {
        match file.read(&mut buf[read_total..]) {
            Ok(0) => break,
            Ok(n) => read_total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    Some(sha256_hex(&buf[..read_total]))
}

fn fingerprint_video_file(path: &Path) -> Option<MediaFingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    let len = meta.len();
    let mtime_ns = file_mtime_ns(&meta)?;
    let mut f = File::open(path).ok()?;
    let head_len = VIDEO_SAMPLE_BYTES.min(len as usize);
    let head_sha256 = read_chunk_sha256(&mut f, 0, head_len)?;
    let mid_off = (len / 2).saturating_sub((VIDEO_SAMPLE_BYTES / 2) as u64);
    let mid_len = VIDEO_SAMPLE_BYTES.min((len - mid_off) as usize);
    let mid_sha256 = read_chunk_sha256(&mut f, mid_off, mid_len)?;
    let tail_off = len.saturating_sub(VIDEO_SAMPLE_BYTES as u64);
    let tail_len = VIDEO_SAMPLE_BYTES.min((len - tail_off) as usize);
    let tail_sha256 = read_chunk_sha256(&mut f, tail_off, tail_len)?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video")
        .to_owned();
    Some(MediaFingerprint::Video {
        filename,
        len,
        mtime_ns,
        head_sha256,
        mid_sha256,
        tail_sha256,
    })
}

fn collect_dir_file_stats(
    root: &Path,
    base: &Path,
    out: &mut Vec<(String, u64, u128)>,
) -> Option<()> {
    if !root.exists() {
        return Some(());
    }
    if !root.is_dir() {
        return None;
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            collect_dir_file_stats(&path, base, out)?;
        } else if path.is_file() {
            let meta = entry.metadata().ok()?;
            let mtime_ns = file_mtime_ns(&meta)?;
            let rel = path
                .strip_prefix(base)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, meta.len(), mtime_ns));
        }
    }
    Some(())
}

pub fn fingerprint_dataset_dir(data_dir: &Path, data_source: &str) -> Option<DatasetCacheFingerprint> {
    let mut files = BTreeMap::new();
    for name in DATASET_FILENAMES.iter().copied().chain(std::iter::once("underground_db.json")) {
        let h = hash_optional_present_file(&data_dir.join(name))?;
        files.insert(name.to_owned(), h);
    }
    Some(DatasetCacheFingerprint {
        data_source: data_source.to_owned(),
        files,
    })
}

pub fn build_run_env_fingerprint(
    ffmpeg_path: Option<&Path>,
    gt_file: &Path,
    baseline_path: &Path,
    effective_thresholds: &QaThresholds,
    overrides: &QaThresholdOverrides,
    current_commit: &str,
) -> Option<QaRunEnvFingerprint> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_sha256 = hash_required_file(&exe_path)?;
    let ffmpeg_sha256 = match ffmpeg_path {
        Some(p) => Some(hash_required_file(p)?),
        None => None,
    };
    let gt_sha256 = hash_optional_present_file(gt_file)?;
    let baseline_sha256 = hash_optional_present_file(baseline_path)?;
    let ocr_lang = WindowsOcrService::try_new()
        .map(|s| s.recognizer_language_tag().to_owned())
        .unwrap_or_else(|_| "none".to_owned());
    Some(QaRunEnvFingerprint {
        exe_sha256,
        ffmpeg_sha256,
        ocr_lang,
        gt_sha256,
        baseline_sha256,
        effective_thresholds: effective_thresholds.clone(),
        overrides: overrides.clone(),
        current_commit: current_commit.trim().to_owned(),
    })
}

pub fn build_session_input_fingerprint(
    session_dir: &Path,
    fallback_data_dir: &Path,
    dataset_cache: &mut HashMap<(PathBuf, String), DatasetCacheFingerprint>,
) -> Option<SessionInputFingerprint> {
    let session_name = session_dir
        .file_name()
        .and_then(|n| n.to_str())?
        .to_owned();
    let (eff_data_dir, data_source) = resolve_replay_data_dir(session_dir, fallback_data_dir);
    let ds_key = (eff_data_dir.clone(), data_source.to_owned());
    let dataset = if let Some(cached) = dataset_cache.get(&ds_key) {
        cached.clone()
    } else {
        let computed = fingerprint_dataset_dir(&eff_data_dir, data_source)?;
        dataset_cache.insert(ds_key, computed.clone());
        computed
    };

    let meta_sha256 = hash_required_file(&session_dir.join("meta.jsonl"))?;
    let info_sha256 = hash_optional_present_file(&session_dir.join("info.json"))?;
    let web_inputs_sha256 =
        hash_optional_present_file(&session_dir.join(WEB_INPUTS_FILENAME))?;
    let blessed_poses_sha256 =
        hash_optional_present_file(&session_dir.join("blessed_poses.jsonl"))?;
    let assets_cache = session_dir.join("assets_cache");
    let paimon_tpl_sha256 =
        hash_optional_present_file(&assets_cache.join("paimon_tpl.npz"))?;

    let media = if session_dir.join("video.mkv").is_file() {
        fingerprint_video_file(&session_dir.join("video.mkv"))?
    } else if session_dir.join("video.mp4").is_file() {
        fingerprint_video_file(&session_dir.join("video.mp4"))?
    } else if session_dir.join("frames").is_dir() {
        let frames_dir = session_dir.join("frames");
        let mut files = Vec::new();
        collect_dir_file_stats(&frames_dir, &frames_dir, &mut files)?;
        files.sort();
        MediaFingerprint::PngFrames { files }
    } else if session_dir.join("01_game.png").is_file() {
        let img_sha256 = hash_required_file(&session_dir.join("01_game.png"))?;
        let report_sha256 =
            hash_optional_present_file(&session_dir.join("03_report.json"))?;
        MediaFingerprint::DiagnosticBundle {
            img_sha256,
            report_sha256,
        }
    } else {
        return None;
    };

    let mut assets_cache_files = Vec::new();
    for sub in ["tiles", "ug_overlays"] {
        let dir = assets_cache.join(sub);
        if dir.exists() {
            collect_dir_file_stats(&dir, &assets_cache, &mut assets_cache_files)?;
        }
    }
    assets_cache_files.sort();

    Some(SessionInputFingerprint {
        session_name,
        dataset,
        meta_sha256,
        info_sha256,
        web_inputs_sha256,
        blessed_poses_sha256,
        paimon_tpl_sha256,
        media,
        assets_cache_files,
    })
}

pub fn compute_session_engine_unchanged(
    session_dir: &Path,
    fallback_data_dir: &Path,
    current_commit: &str,
    repo_root: Option<&Path>,
) -> Option<bool> {
    let (eff_data_dir, data_source) = resolve_replay_data_dir(session_dir, fallback_data_dir);
    let recorded = read_session_provenance(session_dir);
    let prov = build_replay_provenance_with_repo(
        recorded,
        data_source,
        &eff_data_dir,
        current_commit,
        repo_root,
    );
    prov.engine_code_unchanged
}

#[derive(Serialize)]
struct SessionKeyPayload<'a> {
    schema_version: u32,
    kind: &'static str,
    exe_sha256: &'a str,
    ffmpeg_sha256: Option<&'a str>,
    ocr_lang: &'a str,
    gt_sha256: Option<&'a str>,
    baseline_sha256: Option<&'a str>,
    effective_thresholds: &'a QaThresholds,
    overrides: &'a QaThresholdOverrides,
    current_commit: &'a str,
    engine_code_unchanged: Option<bool>,
    build_git_commit: &'a str,
    session: &'a SessionInputFingerprint,
}

#[derive(Serialize)]
struct ScenarioKeyPayload<'a> {
    schema_version: u32,
    kind: &'static str,
    exe_sha256: &'a str,
    ffmpeg_sha256: Option<&'a str>,
    ocr_lang: &'a str,
    gt_sha256: Option<&'a str>,
    scenario_file_sha256: &'a str,
    session_name: &'a str,
    dataset: &'a DatasetCacheFingerprint,
    meta_sha256: &'a str,
    info_sha256: Option<&'a str>,
    web_inputs_sha256: Option<&'a str>,
    paimon_tpl_sha256: Option<&'a str>,
    media: &'a MediaFingerprint,
    assets_cache_files: &'a [(String, u64, u128)],
}

pub fn session_cache_key(
    env: &QaRunEnvFingerprint,
    session: &SessionInputFingerprint,
    engine_code_unchanged: Option<bool>,
) -> String {
    let payload = SessionKeyPayload {
        schema_version: QA_CACHE_SCHEMA_VERSION,
        kind: "session",
        exe_sha256: &env.exe_sha256,
        ffmpeg_sha256: env.ffmpeg_sha256.as_deref(),
        ocr_lang: &env.ocr_lang,
        gt_sha256: env.gt_sha256.as_deref(),
        baseline_sha256: env.baseline_sha256.as_deref(),
        effective_thresholds: &env.effective_thresholds,
        overrides: &env.overrides,
        current_commit: &env.current_commit,
        engine_code_unchanged,
        build_git_commit: BUILD_GIT_COMMIT,
        session,
    };
    let bytes = serde_json::to_vec(&payload).expect("serialize session cache key");
    sha256_hex(&bytes)
}

pub fn scenario_cache_key(
    env: &QaRunEnvFingerprint,
    session: &SessionInputFingerprint,
    scenario: &ScenarioFile,
) -> Option<String> {
    let scenario_file_sha256 = scenario.file_sha256.as_deref()?;
    let payload = ScenarioKeyPayload {
        schema_version: QA_CACHE_SCHEMA_VERSION,
        kind: "scenario",
        exe_sha256: &env.exe_sha256,
        ffmpeg_sha256: env.ffmpeg_sha256.as_deref(),
        ocr_lang: &env.ocr_lang,
        gt_sha256: env.gt_sha256.as_deref(),
        scenario_file_sha256,
        session_name: &session.session_name,
        dataset: &session.dataset,
        meta_sha256: &session.meta_sha256,
        info_sha256: session.info_sha256.as_deref(),
        web_inputs_sha256: session.web_inputs_sha256.as_deref(),
        paimon_tpl_sha256: session.paimon_tpl_sha256.as_deref(),
        media: &session.media,
        assets_cache_files: &session.assets_cache_files,
    };
    let bytes = serde_json::to_vec(&payload).expect("serialize scenario cache key");
    Some(sha256_hex(&bytes))
}

pub fn load_cached_session(cache_dir: &Path, key: &str) -> Option<QaSessionResult> {
    let path = cache_dir.join(format!("sess_{key}.json"));
    if !path.is_file() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let entry: CachedSessionEntry = serde_json::from_str(&raw).ok()?;
    if entry.schema_version != QA_CACHE_SCHEMA_VERSION || entry.key != key || !entry.result.passed {
        return None;
    }
    Some(entry.result)
}

pub fn store_cached_session(cache_dir: &Path, key: &str, result: &QaSessionResult) {
    if !result.passed {
        return;
    }
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let entry = CachedSessionEntry {
        schema_version: QA_CACHE_SCHEMA_VERSION,
        key: key.to_owned(),
        result: result.clone(),
    };
    if let Ok(val) = serde_json::to_value(&entry) {
        let _ = write_json_atomic(&cache_dir.join(format!("sess_{key}.json")), &val);
    }
}

pub fn load_cached_scenario(cache_dir: &Path, key: &str) -> Option<ScenarioResult> {
    let path = cache_dir.join(format!("scen_{key}.json"));
    if !path.is_file() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let entry: CachedScenarioEntry = serde_json::from_str(&raw).ok()?;
    if entry.schema_version != QA_CACHE_SCHEMA_VERSION || entry.key != key {
        return None;
    }
    if !matches!(
        entry.result.outcome,
        ScenarioOutcome::Pass | ScenarioOutcome::KnownFail
    ) {
        return None;
    }
    Some(entry.result)
}

pub fn store_cached_scenario(cache_dir: &Path, key: &str, result: &ScenarioResult) {
    if !matches!(
        result.outcome,
        ScenarioOutcome::Pass | ScenarioOutcome::KnownFail
    ) {
        return;
    }
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let entry = CachedScenarioEntry {
        schema_version: QA_CACHE_SCHEMA_VERSION,
        key: key.to_owned(),
        result: result.clone(),
    };
    if let Ok(val) = serde_json::to_value(&entry) {
        let _ = write_json_atomic(&cache_dir.join(format!("scen_{key}.json")), &val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qa::{
        LiveMMapLatency, MetaReferenceSummary, SessionHeadingStability,
    };
    use crate::scenario::ScenarioSummary;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "{prefix}_{}_{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_env() -> QaRunEnvFingerprint {
        QaRunEnvFingerprint {
            exe_sha256: "a".repeat(64),
            ffmpeg_sha256: Some("b".repeat(64)),
            ocr_lang: "en-US".to_owned(),
            gt_sha256: Some("c".repeat(64)),
            baseline_sha256: Some("d".repeat(64)),
            effective_thresholds: QaThresholds::default(),
            overrides: QaThresholdOverrides::default(),
            current_commit: "60c41bf".to_owned(),
        }
    }

    fn sample_session_fp() -> SessionInputFingerprint {
        let mut ds_files = BTreeMap::new();
        ds_files.insert("multimap_v4.json".to_owned(), Some("1".repeat(64)));
        SessionInputFingerprint {
            session_name: "sess_demo".to_owned(),
            dataset: DatasetCacheFingerprint {
                data_source: "data_dir".to_owned(),
                files: ds_files,
            },
            meta_sha256: "2".repeat(64),
            info_sha256: Some("3".repeat(64)),
            web_inputs_sha256: Some("4".repeat(64)),
            blessed_poses_sha256: Some("5".repeat(64)),
            paimon_tpl_sha256: None,
            media: MediaFingerprint::Video {
                filename: "video.mkv".to_owned(),
                len: 1024,
                mtime_ns: 999,
                head_sha256: "6".repeat(64),
                mid_sha256: "7".repeat(64),
                tail_sha256: "8".repeat(64),
            },
            assets_cache_files: vec![("tiles/2/0_0.png".to_owned(), 42, 1234)],
        }
    }

    fn sample_session_result(passed: bool) -> QaSessionResult {
        QaSessionResult {
            session_name: "sess_demo".to_owned(),
            passed,
            status: if passed { "ĐẠT" } else { "KHÔNG ĐẠT" }.to_owned(),
            reason: "ok".to_owned(),
            reasons: if passed { vec![] } else { vec!["fail".to_owned()] },
            notes: vec![],
            frames: 100,
            codec: "libx264rgb".to_owned(),
            color_safe: true,
            wall_elapsed_s: 1.5,
            effective_fps: 66.7,
            hd_downloads: 0,
            map_m_synced_count: 1,
            baseline_map_m_synced_count: Some(1),
            mmap_view_frames: 10,
            mmap_view_acquired: 10,
            baseline_mmap_view_acquired: Some(10),
            mmap_view_drawer_open: 0,
            blessed_sha256_ok: true,
            vs_blessed: None,
            vs_meta_ref: MetaReferenceSummary::default(),
            heading_stability: SessionHeadingStability::default(),
            live_mmap_latency: LiveMMapLatency::default(),
            provenance: None,
            recording_fidelity: None,
            input_replay: None,
            truth: None,
        }
    }

    #[test]
    fn cache_key_changes_when_any_input_changes() {
        let env = sample_env();
        let sess = sample_session_fp();
        let base_key = session_cache_key(&env, &sess, Some(true));

        let mut env_exe = env.clone();
        env_exe.exe_sha256 = "z".repeat(64);
        assert_ne!(base_key, session_cache_key(&env_exe, &sess, Some(true)));

        let mut env_ff = env.clone();
        env_ff.ffmpeg_sha256 = None;
        assert_ne!(base_key, session_cache_key(&env_ff, &sess, Some(true)));

        let mut env_ocr = env.clone();
        env_ocr.ocr_lang = "vi-VN".to_owned();
        assert_ne!(base_key, session_cache_key(&env_ocr, &sess, Some(true)));

        let mut env_gt = env.clone();
        env_gt.gt_sha256 = Some("y".repeat(64));
        assert_ne!(base_key, session_cache_key(&env_gt, &sess, Some(true)));

        let mut env_base = env.clone();
        env_base.baseline_sha256 = Some("x".repeat(64));
        assert_ne!(base_key, session_cache_key(&env_base, &sess, Some(true)));

        let mut env_th = env.clone();
        env_th.effective_thresholds.pos_threshold_w = 10.0;
        assert_ne!(base_key, session_cache_key(&env_th, &sess, Some(true)));

        let mut env_ov = env.clone();
        env_ov.overrides.max_pos_delta_w = Some(5.0);
        assert_ne!(base_key, session_cache_key(&env_ov, &sess, Some(true)));

        let mut env_commit = env.clone();
        env_commit.current_commit = "deadbeef".to_owned();
        assert_ne!(base_key, session_cache_key(&env_commit, &sess, Some(true)));

        assert_ne!(base_key, session_cache_key(&env, &sess, Some(false)));

        let mut sess_ds = sess.clone();
        sess_ds
            .dataset
            .files
            .insert("multimap_v4.json".to_owned(), Some("9".repeat(64)));
        assert_ne!(base_key, session_cache_key(&env, &sess_ds, Some(true)));

        let mut sess_meta = sess.clone();
        sess_meta.meta_sha256 = "8".repeat(64);
        assert_ne!(base_key, session_cache_key(&env, &sess_meta, Some(true)));

        let mut sess_blessed = sess.clone();
        sess_blessed.blessed_poses_sha256 = Some("7".repeat(64));
        assert_ne!(base_key, session_cache_key(&env, &sess_blessed, Some(true)));

        let mut sess_vid = sess.clone();
        if let MediaFingerprint::Video { ref mut tail_sha256, .. } = sess_vid.media {
            *tail_sha256 = "f".repeat(64);
        }
        assert_ne!(base_key, session_cache_key(&env, &sess_vid, Some(true)));

        let mut sess_tile = sess.clone();
        sess_tile.assets_cache_files.push(("tiles/2/1_1.png".to_owned(), 10, 1));
        assert_ne!(base_key, session_cache_key(&env, &sess_tile, Some(true)));
    }

    #[test]
    fn cache_roundtrip_stores_passing_only_and_rejects_failures() {
        let tmp = TempDir::new("gh_qa_cache_rt");
        let env = sample_env();
        let sess = sample_session_fp();
        let key = session_cache_key(&env, &sess, Some(true));

        assert!(load_cached_session(&tmp.0, &key).is_none());

        // Failing results must never be cached
        let fail_res = sample_session_result(false);
        store_cached_session(&tmp.0, &key, &fail_res);
        assert!(load_cached_session(&tmp.0, &key).is_none());

        // Passing results round-trip identically
        let pass_res = sample_session_result(true);
        store_cached_session(&tmp.0, &key, &pass_res);
        let hit = load_cached_session(&tmp.0, &key).expect("expected cache hit");
        assert_eq!(hit, pass_res);

        // Scenario cache: Pass & KnownFail cached; Fail & Xpass skipped
        let sc = ScenarioFile {
            schema_version: 1,
            name: "sc".to_owned(),
            session: "sess_demo".to_owned(),
            description: "desc".to_owned(),
            known_bug: None,
            inputs: vec![],
            expect: vec![],
            file_sha256: Some("e".repeat(64)),
        };
        let sc_key = scenario_cache_key(&env, &sess, &sc).unwrap();
        let mk_sc_res = |outcome: ScenarioOutcome| ScenarioResult {
            name: "sc".to_owned(),
            session: "sess_demo".to_owned(),
            description: "desc".to_owned(),
            known_bug: None,
            outcome,
            frames_replayed: 5,
            wall_elapsed_s: 0.2,
            checks_passed: 1,
            checks_total: 1,
            first_failure: None,
            checks: vec![],
        };

        for non_cached in [ScenarioOutcome::Fail, ScenarioOutcome::Xpass] {
            store_cached_scenario(&tmp.0, &sc_key, &mk_sc_res(non_cached));
            assert!(load_cached_scenario(&tmp.0, &sc_key).is_none());
        }
        let sc_pass = mk_sc_res(ScenarioOutcome::Pass);
        store_cached_scenario(&tmp.0, &sc_key, &sc_pass);
        assert_eq!(load_cached_scenario(&tmp.0, &sc_key), Some(sc_pass));

        let mut sc2 = sc.clone();
        sc2.file_sha256 = Some("f".repeat(64));
        let sc_key2 = scenario_cache_key(&env, &sess, &sc2).unwrap();
        assert_ne!(sc_key, sc_key2);
        let sc_kf = mk_sc_res(ScenarioOutcome::KnownFail);
        store_cached_scenario(&tmp.0, &sc_key2, &sc_kf);
        assert_eq!(load_cached_scenario(&tmp.0, &sc_key2), Some(sc_kf));
        let _ = ScenarioSummary::from_results(&[]);
    }
}
