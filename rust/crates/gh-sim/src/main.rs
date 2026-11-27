//! Dev-only CLI (`gh-sim`) for deterministic offline replay and diagnostic GT import.

use clap::{Parser, Subcommand};
use gh_sim::compact::{CompactOptions, CompactProfile, run_compact};
use gh_sim::import_diag::{ImportDiagArgs, import_diagnostic_bundle};
use gh_sim::qa::{QaOptions, QaThresholdOverrides, run_qa};
use gh_sim::replay::{DEFAULT_DRIFT_THRESHOLD_W, ReplayOptions, run_replay};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "gh-sim",
    about = "Genshin-Helper deterministic offline replay harness & diagnostic GT importer (dev-only)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Replay a recorded session or diagnostic bundle synchronously through production run_tick_worker.
    Replay {
        /// Path to session directory (minimap_rec, lossless video session, or diag_* bundle).
        source: PathBuf,

        /// Path to map datasets directory containing multimap_v4.json & terrain_n2_v4.npz.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,

        /// Path to git-tracked verified_gt.json.
        #[arg(long, default_value = "tests/verified_gt.json")]
        gt_file: PathBuf,

        /// Optional offline HD tiles directory (overridden by <source>/assets_cache/tiles if present).
        #[arg(long)]
        tiles_dir: Option<PathBuf>,

        /// Optional offline underground overlays directory (overridden by <source>/assets_cache/ug_overlays if present).
        #[arg(long)]
        ug_dir: Option<PathBuf>,

        /// Optional offline paimon_tpl.npz cache file (overridden by <source>/assets_cache/paimon_tpl.npz if present).
        #[arg(long)]
        paimon_cache: Option<PathBuf>,

        /// Explicit absolute path to ffmpeg.exe (required for lossless video sessions; never resolved via PATH).
        #[arg(long)]
        ffmpeg: Option<PathBuf>,

        /// Optional maximum number of frames to process.
        #[arg(long)]
        max_frames: Option<usize>,

        /// Position delta threshold (world units) for regression drift-rate and streak metrics.
        #[arg(long, default_value_t = DEFAULT_DRIFT_THRESHOLD_W)]
        drift_threshold_w: f64,

        /// Exercise synchronous MMapViewEngine registration during M-Map ticks.
        #[arg(long)]
        verify_mmap_view: bool,

        /// Directory to save composited HUD/Radar preview PNGs overlaid on real frames.
        #[arg(long)]
        render_out: Option<PathBuf>,

        /// Write structured report.json to this path.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Copy an Alt+F12 diagnostic folder into scratch/diagnostics/ and record human-verified GT in tests/verified_gt.json.
    ImportDiag {
        /// Source diag_* directory to import.
        source: PathBuf,

        /// Destination diagnostics directory.
        #[arg(long, default_value = "scratch/diagnostics")]
        diagnostics_root: PathBuf,

        /// Git-tracked verified_gt.json path.
        #[arg(long, default_value = "tests/verified_gt.json")]
        gt_file: PathBuf,

        /// Mark bundle as non-M-Map / edge-compass that MUST be rejected (zero false-lock).
        #[arg(long)]
        expect_rejected: bool,

        /// Expected map_id for valid M-Map diagnostic bundles.
        #[arg(long)]
        map_id: Option<i32>,

        /// Optional human-verified world_x coordinate (must be paired with --y).
        #[arg(long, allow_hyphen_values = true)]
        x: Option<f64>,

        /// Optional human-verified world_y coordinate (must be paired with --x).
        #[arg(long, allow_hyphen_values = true)]
        y: Option<f64>,

        /// Short human note explaining this regression case.
        #[arg(long, default_value = "")]
        note: String,

        /// Overwrite existing destination directory or GT entry.
        #[arg(long)]
        force: bool,
    },

    /// Losslessly re-encode and/or trim a recorded video session (keeps 100% bit-exact RGB pixels).
    Compact {
        /// Source video session directory containing video.mkv & meta.jsonl.
        source: PathBuf,

        /// Explicit absolute path to ffmpeg.exe (never resolved via PATH).
        #[arg(long)]
        ffmpeg: PathBuf,

        /// Destination session directory (default: <source>_compact).
        #[arg(long)]
        out: Option<PathBuf>,

        /// First frame to keep (1-based inclusive, default 1).
        #[arg(long, default_value_t = 1)]
        start_frame: usize,

        /// Last frame to keep (1-based inclusive, default last frame).
        #[arg(long)]
        end_frame: Option<usize>,

        /// Compaction profile: archive (libx265 4:4:4 crf 22, ~39x smaller), near-lossless (libx264rgb crf 18), or lossless (libx264rgb qp 0).
        #[arg(long, default_value = "archive")]
        profile: String,

        /// Optional CRF override for archive (default 22) or near-lossless (default 18).
        #[arg(long)]
        crf: Option<u8>,

        /// Encoder speed/compression preset (e.g. fast, medium, slow).
        #[arg(long, default_value = "fast")]
        preset: String,

        /// Overwrite existing destination directory.
        #[arg(long)]
        force: bool,
    },

    /// Run sequential offline QA regression checks across all recorded sessions against blessed poses.
    Qa {
        /// Directory containing recorded QA sessions (default: scratch/qa_sessions).
        #[arg(long, default_value = "scratch/qa_sessions")]
        sessions: PathBuf,

        /// Path to map datasets directory containing multimap_v4.json & terrain_n2_v4.npz.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,

        /// Explicit absolute path to ffmpeg.exe (required for video sessions; never resolved via PATH).
        #[arg(long)]
        ffmpeg: Option<PathBuf>,

        /// Path to git-tracked QA baseline JSON (default: tests/qa_baseline.json).
        #[arg(long, default_value = "tests/qa_baseline.json")]
        baseline: PathBuf,

        /// Path to git-tracked verified_gt.json.
        #[arg(long, default_value = "tests/verified_gt.json")]
        gt_file: PathBuf,

        /// Optional session name filter (repeatable: --only sess_A --only sess_B).
        #[arg(long)]
        only: Vec<String>,

        /// Optional output directory where qa_report.json will be written.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Record fresh blessed_poses.jsonl and update tests/qa_baseline.json (requires explicit user approval).
        #[arg(long)]
        bless: bool,

        /// Optional CLI override for per-frame position drift threshold (world units).
        #[arg(long, allow_hyphen_values = true)]
        pos_threshold_w: Option<f64>,

        /// Optional CLI override for max frames allowed above pos-threshold-w.
        #[arg(long)]
        max_over_threshold: Option<usize>,

        /// Optional CLI override for max map_id mismatches vs blessed.
        #[arg(long)]
        max_map_mismatches: Option<usize>,

        /// Optional CLI override for max frames that lost tracking vs blessed.
        #[arg(long)]
        max_lost_pos_frames: Option<usize>,

        /// Optional CLI override for max drop in map_m_synced_count vs baseline.
        #[arg(long)]
        max_map_m_synced_drop: Option<usize>,

        /// Optional CLI override for max drop in mmap_view_acquired vs baseline.
        #[arg(long)]
        max_mmap_view_drop: Option<usize>,

        /// Optional CLI stricter floor for mmap_view_acquired per session.
        #[arg(long)]
        min_mmap_view_acquired: Option<usize>,

        /// Optional CLI stricter ceiling for p95 position delta vs blessed (world units).
        #[arg(long, allow_hyphen_values = true)]
        max_p95_pos_delta_w: Option<f64>,

        /// Optional CLI stricter ceiling for max position delta vs blessed (world units).
        #[arg(long, allow_hyphen_values = true)]
        max_pos_delta_w: Option<f64>,
    },
}

fn main() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(env_filter).try_init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Replay {
            source,
            data_dir,
            gt_file,
            tiles_dir,
            ug_dir,
            paimon_cache,
            ffmpeg,
            max_frames,
            drift_threshold_w,
            verify_mmap_view,
            render_out,
            out,
        } => {
            let res = run_replay(&ReplayOptions {
                source_dir: source,
                data_dir,
                gt_file,
                tiles_dir,
                ug_dir,
                paimon_cache_path: paimon_cache,
                ffmpeg_path: ffmpeg,
                max_frames,
                drift_threshold_w,
                verify_mmap_view,
                render_out,
                out_report: out.clone(),
            });
            match res {
                Ok(report) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).expect("serialize replay report")
                    );
                    if report.accuracy.has_verified_gt && report.accuracy.false_lock_count > 0 {
                        std::process::exit(1);
                    }
                }
                Err(err) => {
                    eprintln!("[gh-sim replay] ERROR: {err}");
                    std::process::exit(2);
                }
            }
        }
        Cmd::ImportDiag {
            source,
            diagnostics_root,
            gt_file,
            expect_rejected,
            map_id,
            x,
            y,
            note,
            force,
        } => {
            let ref_xy = match (x, y) {
                (Some(wx), Some(wy)) if wx.is_finite() && wy.is_finite() => Some([wx, wy]),
                (None, None) => None,
                _ => {
                    eprintln!("[gh-sim import-diag] ERROR: --x and --y must be specified together");
                    std::process::exit(2);
                }
            };
            match import_diagnostic_bundle(&ImportDiagArgs {
                source_dir: source,
                diagnostics_root,
                gt_file,
                expect_rejected,
                expected_map_id: map_id,
                ref_xy,
                note,
                force,
            }) {
                Ok(outcome) => {
                    println!(
                        "Imported {} ({} files copied -> {}), updated {}",
                        outcome.bundle_name,
                        outcome.files_copied,
                        outcome.dest_dir.display(),
                        outcome.gt_file.display()
                    );
                }
                Err(err) => {
                    eprintln!("[gh-sim import-diag] ERROR: {err}");
                    std::process::exit(2);
                }
            }
        }
        Cmd::Compact {
            source,
            ffmpeg,
            out,
            start_frame,
            end_frame,
            profile,
            crf,
            preset,
            force,
        } => {
            let parsed_profile = match CompactProfile::parse(&profile) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("[gh-sim compact] ERROR: {err}");
                    std::process::exit(2);
                }
            };
            match run_compact(&CompactOptions {
                source_dir: source,
                out_dir: out,
                ffmpeg_path: ffmpeg,
                start_frame,
                end_frame,
                profile: parsed_profile,
                crf,
                preset,
                force,
            }) {
                Ok(report) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).expect("serialize compact report")
                    );
                }
                Err(err) => {
                    eprintln!("[gh-sim compact] ERROR: {err}");
                    std::process::exit(2);
                }
            }
        }
        Cmd::Qa {
            sessions,
            data_dir,
            ffmpeg,
            baseline,
            gt_file,
            only,
            out,
            bless,
            pos_threshold_w,
            max_over_threshold,
            max_map_mismatches,
            max_lost_pos_frames,
            max_map_m_synced_drop,
            max_mmap_view_drop,
            min_mmap_view_acquired,
            max_p95_pos_delta_w,
            max_pos_delta_w,
        } => {
            let res = run_qa(&QaOptions {
                sessions_dir: sessions,
                data_dir,
                ffmpeg_path: ffmpeg,
                baseline_path: baseline,
                gt_file,
                only,
                out_dir: out,
                bless,
                overrides: QaThresholdOverrides {
                    pos_threshold_w,
                    max_over_threshold_frames: max_over_threshold,
                    max_map_mismatches,
                    max_lost_pos_frames,
                    max_map_m_synced_drop,
                    max_mmap_view_drop,
                    min_mmap_view_acquired,
                    max_p95_pos_delta_w,
                    max_pos_delta_w,
                },
            });
            match res {
                Ok(report) => {
                    if !report.passed {
                        std::process::exit(1);
                    }
                }
                Err(err) => {
                    eprintln!("[gh-sim qa] ERROR: {err}");
                    std::process::exit(2);
                }
            }
        }
    }
}
