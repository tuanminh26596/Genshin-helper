//! Lossless offline compaction & synchronized trimming for recorded video sessions.
//!
//! Trims `video.mkv` and `meta.jsonl` together to `[start_frame..=end_frame]` (1-based inclusive),
//! re-encodes with `libx264rgb -qp 0 -preset <preset>` (bit-exact lossless RGB), copies
//! `assets_cache/`, and writes an updated `info.json` into a new output directory.

use gh_app::session_recorder::validate_ffmpeg_path;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ALLOWED_PRESETS: &[&str] = &[
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactProfile {
    /// Bit-exact lossless RGB (`libx264rgb -qp 0`, ~25–30% smaller than ultrafast).
    Lossless,
    /// Visually lossless 4:4:4 RGB (`libx264rgb -crf 18`, ~10× smaller).
    NearLossless,
    /// Extreme long-term QA archive (`libx265 -pix_fmt yuv444p -crf 22`, ~35–40× smaller,
    /// preserves full 4:4:4 chroma for cyan arrow/waypoint/underground CV edges).
    Archive,
}

impl CompactProfile {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lossless" => Ok(Self::Lossless),
            "near-lossless" | "near_lossless" | "rgb" => Ok(Self::NearLossless),
            "archive" | "hevc" | "x265" => Ok(Self::Archive),
            other => Err(format!(
                "unsupported compact profile '{other}'; expected one of: archive, near-lossless, lossless"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::NearLossless => "near-lossless",
            Self::Archive => "archive",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactOptions {
    pub source_dir: PathBuf,
    pub out_dir: Option<PathBuf>,
    pub ffmpeg_path: PathBuf,
    pub start_frame: usize,
    pub end_frame: Option<usize>,
    pub profile: CompactProfile,
    pub crf: Option<u8>,
    pub preset: String,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactReport {
    pub source_dir: String,
    pub out_dir: String,
    pub profile: String,
    pub codec: String,
    pub preset: String,
    pub crf: Option<u8>,
    pub start_frame: usize,
    pub end_frame: usize,
    pub source_frames: usize,
    pub kept_frames: usize,
    pub source_bytes: u64,
    pub output_bytes: u64,
    pub source_mb: f64,
    pub output_mb: f64,
    pub saved_pct: f64,
}

fn validate_preset(preset: &str) -> Result<&'static str, String> {
    let norm = preset.trim().to_ascii_lowercase();
    ALLOWED_PRESETS
        .iter()
        .copied()
        .find(|&p| p == norm)
        .ok_or_else(|| {
            format!(
                "unsupported x264 preset '{preset}'; expected one of: {}",
                ALLOWED_PRESETS.join(", ")
            )
        })
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<usize, String> {
    if !src.is_dir() {
        return Ok(0);
    }
    fs::create_dir_all(dst)
        .map_err(|e| format!("failed to create directory {}: {e}", dst.display()))?;
    let mut copied = 0usize;
    let rd = fs::read_dir(src)
        .map_err(|e| format!("failed to read directory {}: {e}", src.display()))?;
    for entry in rd.flatten() {
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if p.is_dir() {
            copied += copy_dir_recursive(&p, &target)?;
        } else if p.is_file() {
            fs::copy(&p, &target)
                .map_err(|e| format!("failed to copy {} -> {}: {e}", p.display(), target.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

pub fn default_compact_out_dir(source_dir: &Path) -> PathBuf {
    let name = source_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    let parent = source_dir.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{name}_compact"))
}

pub fn run_compact(opts: &CompactOptions) -> Result<CompactReport, String> {
    let ffmpeg_abs = validate_ffmpeg_path(Some(&opts.ffmpeg_path))
        .map_err(|e| format!("invalid --ffmpeg: {e}"))?;
    let preset = validate_preset(&opts.preset)?;

    if !opts.source_dir.is_dir() {
        return Err(format!(
            "source session directory does not exist: {}",
            opts.source_dir.display()
        ));
    }
    let src_video = opts.source_dir.join("video.mkv");
    let src_meta = opts.source_dir.join("meta.jsonl");
    if !src_video.is_file() {
        return Err(format!("missing source video: {}", src_video.display()));
    }
    if !src_meta.is_file() {
        return Err(format!("missing source meta.jsonl: {}", src_meta.display()));
    }

    let out_dir = opts
        .out_dir
        .clone()
        .unwrap_or_else(|| default_compact_out_dir(&opts.source_dir));
    if out_dir == opts.source_dir {
        return Err("output directory must differ from source directory (non-destructive compact)".to_owned());
    }
    if out_dir.exists() {
        if opts.force {
            fs::remove_dir_all(&out_dir)
                .map_err(|e| format!("failed to clear existing {}: {e}", out_dir.display()))?;
        } else {
            return Err(format!(
                "output directory already exists: {} (pass --force or choose --out)",
                out_dir.display()
            ));
        }
    }

    // Load and validate meta.jsonl rows
    let meta_file = File::open(&src_meta)
        .map_err(|e| format!("failed to open {}: {e}", src_meta.display()))?;
    let mut rows: Vec<Value> = Vec::new();
    for (line_no, line) in BufReader::new(meta_file).lines().enumerate() {
        let line = line.map_err(|e| format!("read error at meta.jsonl:{}: {e}", line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid JSON at meta.jsonl:{}: {e}", line_no + 1))?;
        rows.push(val);
    }
    let source_frames = rows.len();
    if source_frames == 0 {
        return Err("source meta.jsonl has 0 frames".to_owned());
    }
    let start_frame = opts.start_frame.max(1);
    let end_frame = opts.end_frame.unwrap_or(source_frames).min(source_frames);
    if start_frame > end_frame {
        return Err(format!(
            "invalid frame range: start_frame ({start_frame}) > end_frame ({end_frame}) [source has {source_frames} frames]"
        ));
    }

    let src_info_path = opts.source_dir.join("info.json");
    let mut info_val: Value = if src_info_path.is_file() {
        fs::read_to_string(&src_info_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };
    let fps = info_val
        .get("fps")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(20.0);

    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("failed to create {}: {e}", out_dir.display()))?;

    // Copy assets_cache/ so compacted session stays 100% hermetic offline
    let _ = copy_dir_recursive(
        &opts.source_dir.join("assets_cache"),
        &out_dir.join("assets_cache"),
    )?;

    // Write trimmed & re-indexed meta.jsonl
    let kept_slice = &rows[(start_frame - 1)..end_frame];
    let kept_frames = kept_slice.len();
    let out_meta_path = out_dir.join("meta.jsonl");
    {
        let f = File::create(&out_meta_path)
            .map_err(|e| format!("failed to create {}: {e}", out_meta_path.display()))?;
        let mut w = BufWriter::new(f);
        for (idx0, row) in kept_slice.iter().enumerate() {
            let mut cloned = row.clone();
            if let Some(obj) = cloned.as_object_mut() {
                obj.insert("i".to_owned(), json!(idx0 + 1));
            }
            serde_json::to_writer(&mut w, &cloned)
                .map_err(|e| format!("meta.jsonl serialize error: {e}"))?;
            w.write_all(b"\n")
                .map_err(|e| format!("meta.jsonl write error: {e}"))?;
        }
        w.flush()
            .map_err(|e| format!("meta.jsonl flush error: {e}"))?;
    }

    // Re-encode video.mkv according to the selected QA compaction profile
    let out_video = out_dir.join("video.mkv");
    let s0 = start_frame - 1;
    let e0 = end_frame - 1;
    let is_trimmed = start_frame > 1 || end_frame < source_frames;
    let (codec_name, eff_crf): (&str, Option<u8>) = match opts.profile {
        CompactProfile::Lossless => ("libx264rgb", None),
        CompactProfile::NearLossless => ("libx264rgb", Some(opts.crf.unwrap_or(18).min(30))),
        CompactProfile::Archive => ("libx265", Some(opts.crf.unwrap_or(22).min(32))),
    };

    let mut cmd = Command::new(&ffmpeg_abs);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&src_video);
    if is_trimmed {
        let vf = format!("select='between(n\\,{s0}\\,{e0})',setpts=N/({fps:.6}*TB)");
        cmd.args(["-vf", &vf, "-fps_mode", "passthrough"]);
    }
    match opts.profile {
        CompactProfile::Lossless => {
            cmd.args([
                "-c:v",
                "libx264rgb",
                "-qp",
                "0",
                "-preset",
                preset,
                "-pix_fmt",
                "bgr24",
            ]);
        }
        CompactProfile::NearLossless => {
            let crf_s = eff_crf.unwrap_or(18).to_string();
            cmd.args([
                "-c:v",
                "libx264rgb",
                "-crf",
                &crf_s,
                "-preset",
                preset,
                "-pix_fmt",
                "bgr24",
            ]);
        }
        CompactProfile::Archive => {
            let crf_s = eff_crf.unwrap_or(22).to_string();
            cmd.args([
                "-c:v",
                "libx265",
                "-crf",
                &crf_s,
                "-preset",
                preset,
                "-pix_fmt",
                "yuv444p",
                "-x265-params",
                "log-level=error",
            ]);
        }
    }
    cmd.args(["-an"])
        .arg(&out_video)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| format!("failed to launch ffmpeg {}: {e}", ffmpeg_abs.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffmpeg compact failed (exit {:?}): {}",
            output.status.code(),
            stderr.trim()
        ));
    }

    let source_bytes = fs::metadata(&src_video).map(|m| m.len()).unwrap_or(0);
    let output_bytes = fs::metadata(&out_video).map(|m| m.len()).unwrap_or(0);
    let source_mb = ((source_bytes as f64 / (1024.0 * 1024.0)) * 100.0).round() / 100.0;
    let output_mb = ((output_bytes as f64 / (1024.0 * 1024.0)) * 100.0).round() / 100.0;
    let saved_pct = if source_bytes > 0 {
        (((1.0 - (output_bytes as f64 / source_bytes as f64)) * 100.0) * 100.0).round() / 100.0
    } else {
        0.0
    };

    update_compacted_info(
        &mut info_val,
        kept_frames,
        output_bytes,
        codec_name,
        opts.profile,
        eff_crf,
        preset,
        &opts.source_dir,
        start_frame,
        end_frame,
    );
    let out_info = out_dir.join("info.json");
    let info_bytes = serde_json::to_vec_pretty(&info_val)
        .map_err(|e| format!("failed to serialize info.json: {e}"))?;
    fs::write(&out_info, info_bytes)
        .map_err(|e| format!("failed to write {}: {e}", out_info.display()))?;

    Ok(CompactReport {
        source_dir: opts.source_dir.display().to_string(),
        out_dir: out_dir.display().to_string(),
        profile: opts.profile.as_str().to_owned(),
        codec: codec_name.to_owned(),
        preset: preset.to_owned(),
        crf: eff_crf,
        start_frame,
        end_frame,
        source_frames,
        kept_frames,
        source_bytes,
        output_bytes,
        source_mb,
        output_mb,
        saved_pct,
    })
}

#[allow(clippy::too_many_arguments)]
fn update_compacted_info(
    info_val: &mut Value,
    kept_frames: usize,
    output_bytes: u64,
    codec_name: &str,
    profile: CompactProfile,
    eff_crf: Option<u8>,
    preset: &str,
    source_dir: &Path,
    start_frame: usize,
    end_frame: usize,
) {
    if let Some(obj) = info_val.as_object_mut() {
        obj.insert("frames".to_owned(), json!(kept_frames));
        obj.insert("video_size_bytes".to_owned(), json!(output_bytes));
        obj.insert("codec".to_owned(), json!(codec_name));
        obj.insert("compact_profile".to_owned(), json!(profile.as_str()));
        obj.insert("crf".to_owned(), json!(eff_crf));
        obj.insert("preset".to_owned(), json!(preset));
        obj.insert(
            "compacted_from".to_owned(),
            json!(source_dir.file_name().and_then(|n| n.to_str()).unwrap_or("")),
        );
        obj.insert("trimmed_range".to_owned(), json!([start_frame, end_frame]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_validation_accepts_known_and_rejects_unknown() {
        assert_eq!(validate_preset("slow").unwrap(), "slow");
        assert_eq!(validate_preset("VERYSLOW").unwrap(), "veryslow");
        assert!(validate_preset("lossy").is_err());
    }

    #[test]
    fn default_out_dir_appends_compact_suffix() {
        let p = Path::new("C:\\sessions\\sess_20261127_005004");
        assert_eq!(
            default_compact_out_dir(p),
            PathBuf::from("C:\\sessions\\sess_20261127_005004_compact")
        );
    }

    #[test]
    fn compact_preserves_assets_cache_data_and_info_provenance() {
        use gh_core::provenance::{DATASET_FILENAMES, DatasetFingerprint};

        let root = std::env::temp_dir().join(format!(
            "gh_sim_compact_prov_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("sess_src");
        let dst = root.join("sess_src_compact");
        fs::create_dir_all(src.join("assets_cache").join("data")).unwrap();
        fs::create_dir_all(src.join("assets_cache").join("tiles").join("2").join("0")).unwrap();
        fs::write(
            src.join("assets_cache").join("data").join(DATASET_FILENAMES[0]),
            b"{\"version\":\"4.0.0\",\"maps\":{}}",
        )
        .unwrap();
        fs::write(
            src.join("assets_cache").join("data").join(DATASET_FILENAMES[1]),
            b"TERRAIN",
        )
        .unwrap();
        fs::write(
            src.join("assets_cache").join("tiles").join("2").join("0").join("1_2.webp"),
            b"WEBP",
        )
        .unwrap();

        let copied = copy_dir_recursive(&src.join("assets_cache"), &dst.join("assets_cache"))
            .expect("copy assets_cache");
        assert_eq!(copied, 3);
        assert_eq!(
            fs::read(dst.join("assets_cache").join("data").join(DATASET_FILENAMES[0])).unwrap(),
            b"{\"version\":\"4.0.0\",\"maps\":{}}"
        );
        assert_eq!(
            fs::read(dst.join("assets_cache").join("data").join(DATASET_FILENAMES[1])).unwrap(),
            b"TERRAIN"
        );
        assert!(
            dst.join("assets_cache")
                .join("tiles")
                .join("2")
                .join("0")
                .join("1_2.webp")
                .is_file()
        );

        let mut fp = DatasetFingerprint::default();
        fp.0.insert(DATASET_FILENAMES[0].to_owned(), Some("aaaa".to_owned()));
        fp.0.insert(DATASET_FILENAMES[1].to_owned(), Some("bbbb".to_owned()));
        let provenance = json!({
            "app_version": "5.7.0",
            "git_commit": "0123456789abcdef0123456789abcdef01234567",
            "data_source": "installed",
            "datasets": fp,
        });
        let mut info = json!({
            "schema_version": 1,
            "frames": 100,
            "codec": "libx264rgb",
            "provenance": provenance.clone(),
        });
        update_compacted_info(
            &mut info,
            50,
            4096,
            "libx265",
            CompactProfile::Archive,
            Some(22),
            "fast",
            &src,
            10,
            59,
        );
        assert_eq!(info["frames"], json!(50));
        assert_eq!(info["codec"], json!("libx265"));
        assert_eq!(info["trimmed_range"], json!([10, 59]));
        assert_eq!(info["provenance"], provenance);

        let _ = fs::remove_dir_all(&root);
    }
}
