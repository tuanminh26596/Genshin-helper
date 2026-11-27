//! Clean full-frame lossless gameplay session recorder (`--record-session`, hidden dev flag).
//!
//! Design invariants:
//! - Requires an explicit absolute `--ffmpeg <PATH>` executable (NEVER searches `PATH`).
//! - Defaults to 20 fps (matching the live minimap full-step cadence) to keep 1440p recording
//!   CPU overhead practical alongside the running game.
//! - Encodes zero-chroma-subsampling lossless RGB/BGR (`libx264rgb -qp 0 -preset ultrafast` or
//!   `ffv1 -level 3 -g 1 -pix_fmt bgr0`) into `video.mkv` so 1-2 px cyan/yellow UI features stay exact.
//! - Records `meta.jsonl` with per-frame timestamps, client rect, cursor client coordinates,
//!   capture path (`"gdi"` / `"wgc_hdr"`), dropped-frame counts, live pose metadata, and pressed hotkeys.
//! - Mandatorily snapshots all touched HD terrain tiles, underground overlays, and `paimon_tpl.npz`
//!   into `<session>/assets_cache/` on finish so offline replay matches live assets.

use crate::diagnostics::local_timestamp_compact_and_iso;
use crate::{BUILD_APP_VERSION, BUILD_GIT_COMMIT};
use gh_core::atomic_io::write_json_atomic;
use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::multimap::MultimapDb;
use gh_core::provenance::{
    DATASET_FILENAMES, SessionProvenance, fingerprint_data_dir, sanitize_data_source_label,
};
use gh_core::{CoreError, CoreResult};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const DEFAULT_SESSION_FPS: f64 = 20.0;
const DEFAULT_SESSION_QUEUE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCodec {
    Libx264Rgb,
    Ffv1,
}

impl SessionCodec {
    pub fn parse(raw: &str) -> CoreResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "libx264rgb" | "x264rgb" | "x264" => Ok(Self::Libx264Rgb),
            "ffv1" => Ok(Self::Ffv1),
            other => Err(CoreError::InvalidArgument(format!(
                "unsupported --record-codec '{other}' (expected 'libx264rgb' or 'ffv1')"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Libx264Rgb => "libx264rgb",
            Self::Ffv1 => "ffv1",
        }
    }
}

/// Validates `--ffmpeg` strictly as an existing absolute file path (never resolved from `PATH`).
pub fn validate_ffmpeg_path(path: Option<&Path>) -> CoreResult<PathBuf> {
    let Some(p) = path else {
        return Err(CoreError::InvalidArgument(
            "--record-session requires explicit absolute --ffmpeg <PATH> (PATH lookup is disabled)"
                .to_owned(),
        ));
    };
    if !p.is_absolute() {
        return Err(CoreError::InvalidArgument(format!(
            "--ffmpeg must be an absolute path (PATH lookup is disabled): {}",
            p.display()
        )));
    }
    if !p.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "--ffmpeg file does not exist: {}",
            p.display()
        )));
    }
    Ok(p.to_path_buf())
}

#[derive(Debug, Clone)]
pub struct SessionRecorderConfig {
    pub seconds: f64,
    pub fps: f64,
    pub codec: SessionCodec,
    pub ffmpeg_path: PathBuf,
    pub out_root: PathBuf,
    pub tiles_dir: PathBuf,
    pub ug_overlays_dir: PathBuf,
    pub paimon_cache_path: PathBuf,
    pub data_dir: PathBuf,
    pub data_source: String,
    pub maps_db: Arc<MultimapDb>,
    pub max_queue: usize,
}

#[derive(Debug, Clone)]
pub struct SessionFrameMeta {
    pub client_rect: RectI32,
    pub cursor_client_pos: Option<(f64, f64)>,
    pub capture_path: String,
    pub extra: Map<String, Value>,
}

struct RecItem {
    idx: usize,
    bgr: Arc<FrameBuf>,
    meta: Value,
}

struct RecorderShared {
    queue: VecDeque<RecItem>,
    max_queue: usize,
    stopped: bool,
    frames: usize,
    tick_seq: u64,
    pending_unrecorded_ticks: BTreeMap<String, u64>,
    dropped_queue: usize,
    dropped_resize: usize,
    write_errors: usize,
    capture_paths_seen: BTreeSet<String>,
    pending_hotkeys: Vec<Value>,
    pending_events: Vec<Value>,
    visited_coords: Vec<(i32, f64, f64)>,
    visited_floors: HashSet<(i32, i32)>,
}

struct RecorderState {
    first_ts: Option<f64>,
    last_ts: Option<f64>,
    expected_dims: Option<(u32, u32)>,
    finished: bool,
    worker_started: bool,
    worker: Option<JoinHandle<()>>,
}

pub type SessionDoneCallback = Box<dyn FnOnce(PathBuf, usize) + Send>;

pub struct SessionRecorder {
    /// Target recording duration in wall seconds (`0.0` = unlimited until manual stop).
    seconds: f64,
    fps: f64,
    codec: SessionCodec,
    ffmpeg_path: PathBuf,
    out_dir: PathBuf,
    tiles_dir: PathBuf,
    ug_overlays_dir: PathBuf,
    paimon_cache_path: PathBuf,
    data_dir: PathBuf,
    data_source: String,
    maps_db: Arc<MultimapDb>,
    on_done: Mutex<Option<SessionDoneCallback>>,
    shared: Arc<(Mutex<RecorderShared>, Condvar)>,
    state: Mutex<RecorderState>,
    #[cfg(test)]
    skip_worker_spawn: bool,
}

impl SessionRecorder {
    pub fn new(
        cfg: SessionRecorderConfig,
        on_done: Option<SessionDoneCallback>,
    ) -> CoreResult<Arc<Self>> {
        let ffmpeg_path = validate_ffmpeg_path(Some(&cfg.ffmpeg_path))?;
        let sec = if cfg.seconds.is_finite() && cfg.seconds > 0.0 {
            cfg.seconds
        } else {
            0.0
        };
        let fps = if cfg.fps.is_finite() {
            cfg.fps.clamp(1.0, 60.0)
        } else {
            DEFAULT_SESSION_FPS
        };
        let (stamp, _) = local_timestamp_compact_and_iso();
        let mut out_dir = cfg.out_root.join(format!("sess_{stamp}"));
        if out_dir.exists() {
            for idx in 1..=99u32 {
                let cand = cfg.out_root.join(format!("sess_{stamp}_{idx:02}"));
                if !cand.exists() {
                    out_dir = cand;
                    break;
                }
            }
        }

        Ok(Arc::new(Self {
            seconds: sec,
            fps,
            codec: cfg.codec,
            ffmpeg_path,
            out_dir,
            tiles_dir: cfg.tiles_dir,
            ug_overlays_dir: cfg.ug_overlays_dir,
            paimon_cache_path: cfg.paimon_cache_path,
            data_dir: cfg.data_dir,
            data_source: sanitize_data_source_label(&cfg.data_source),
            maps_db: cfg.maps_db,
            on_done: Mutex::new(on_done),
            shared: Arc::new((
                Mutex::new(RecorderShared {
                    queue: VecDeque::new(),
                    max_queue: cfg.max_queue.clamp(8, DEFAULT_SESSION_QUEUE * 4),
                    stopped: false,
                    frames: 0,
                    tick_seq: 0,
                    pending_unrecorded_ticks: BTreeMap::new(),
                    dropped_queue: 0,
                    dropped_resize: 0,
                    write_errors: 0,
                    capture_paths_seen: BTreeSet::new(),
                    pending_hotkeys: Vec::new(),
                    pending_events: Vec::new(),
                    visited_coords: Vec::new(),
                    visited_floors: HashSet::new(),
                }),
                Condvar::new(),
            )),
            state: Mutex::new(RecorderState {
                first_ts: None,
                last_ts: None,
                expected_dims: None,
                finished: false,
                worker_started: false,
                worker: None,
            }),
            #[cfg(test)]
            skip_worker_spawn: false,
        }))
    }

    #[cfg(test)]
    pub(crate) fn new_dry_run_for_test(
        root: &Path,
        fps: f64,
        max_queue: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            seconds: 0.0,
            fps: fps.clamp(1.0, 60.0),
            codec: SessionCodec::Libx264Rgb,
            ffmpeg_path: std::env::current_exe().unwrap_or_else(|_| root.join("ffmpeg.exe")),
            out_dir: root.join("sess_dry_run"),
            tiles_dir: root.join("tiles"),
            ug_overlays_dir: root.join("ug"),
            paimon_cache_path: root.join("paimon.npz"),
            data_dir: root.to_path_buf(),
            data_source: "dev".to_owned(),
            maps_db: Arc::new(MultimapDb::default()),
            on_done: Mutex::new(None),
            shared: Arc::new((
                Mutex::new(RecorderShared {
                    queue: VecDeque::new(),
                    max_queue: max_queue.max(1),
                    stopped: false,
                    frames: 0,
                    tick_seq: 0,
                    pending_unrecorded_ticks: BTreeMap::new(),
                    dropped_queue: 0,
                    dropped_resize: 0,
                    write_errors: 0,
                    capture_paths_seen: BTreeSet::new(),
                    pending_hotkeys: Vec::new(),
                    pending_events: Vec::new(),
                    visited_coords: Vec::new(),
                    visited_floors: HashSet::new(),
                }),
                Condvar::new(),
            )),
            state: Mutex::new(RecorderState {
                first_ts: None,
                last_ts: None,
                expected_dims: None,
                finished: false,
                worker_started: false,
                worker: None,
            }),
            skip_worker_spawn: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn drain_queued_items_for_test(&self) -> Vec<(usize, Value)> {
        let (lock, _) = &*self.shared;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        sh.queue.drain(..).map(|item| (item.idx, item.meta)).collect()
    }

    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    pub fn fps(&self) -> f64 {
        self.fps
    }

    pub fn active(&self) -> bool {
        !self.state.lock().unwrap_or_else(|e| e.into_inner()).finished
    }

    /// Logs a hotkey pressed during the live session so `meta.jsonl` preserves exact interaction timing.
    pub fn record_hotkey_event(&self, ts: f64, action: &str) {
        if !self.active() {
            return;
        }
        let (lock, _) = &*self.shared;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        sh.pending_hotkeys.push(json!({
            "ts": ((ts * 10_000.0).round()) / 10_000.0,
            "action": action,
        }));
    }

    /// Queues a generic app event attached to the next recorded frame's `events` array.
    /// No-op when the recorder is inactive.
    pub fn record_event(&self, ts: f64, kind: &str, fields: Value) {
        if !self.active() {
            return;
        }
        let entry = format_event_entry(ts, kind, fields);
        let (lock, _) = &*self.shared;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        sh.pending_events.push(entry);
    }

    #[cfg(test)]
    pub(crate) fn take_pending_events_for_test(&self) -> Vec<Value> {
        let (lock, _) = &*self.shared;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut sh.pending_events)
    }

    #[cfg(test)]
    pub(crate) fn take_pending_hotkeys_for_test(&self) -> Vec<Value> {
        let (lock, _) = &*self.shared;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut sh.pending_hotkeys)
    }

    /// Records a tracking-loop tick that produced a tick output while recording was active
    /// but was not kept in `video.mkv` / `meta.jsonl`. Increments `tick_seq` and the per-mode
    /// `unrecorded_ticks` bucket that will be flushed onto the next kept frame.
    /// No-op when the recorder is inactive.
    pub fn record_unrecorded_tick(&self, mode_key: &str) {
        if !self.active() {
            return;
        }
        let (lock, _) = &*self.shared;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        sh.tick_seq += 1;
        *sh.pending_unrecorded_ticks
            .entry(mode_key.to_owned())
            .or_insert(0) += 1;
    }

    /// Returns `true` if enough time elapsed since the last captured session frame to take a new one.
    pub fn should_capture_at(&self, ts: f64) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.finished {
            return false;
        }
        match st.last_ts {
            None => true,
            Some(prev) => (ts - prev) >= (1.0 / self.fps) * 0.9,
        }
    }

    /// Enqueues a frame into `video.mkv` + `meta.jsonl`.
    ///
    /// When `force` is `true`, bypasses the `0.9 / fps` inter-frame gate (used to preserve every
    /// M-Map `Settling` tick without dropping transitions). When `force` is `false`, rejects frames
    /// arriving within `0.9 / fps` of the previous kept frame. Returns `true` if the frame was kept.
    pub fn add_frame(
        self: &Arc<Self>,
        frame: &FrameBuf,
        ts: f64,
        fmeta: SessionFrameMeta,
        force: bool,
    ) -> bool {
        self.enqueue_frame_inner(frame, None, ts, fmeta, force)
    }

    /// Like [`Self::add_frame`], but reuses `frame` via `Arc::clone` with zero pixel copy when the
    /// input is already `PixelFormat::Bgr8` (the format produced by M-Map `capture_region`).
    pub fn add_frame_arc(
        self: &Arc<Self>,
        frame: &Arc<FrameBuf>,
        ts: f64,
        fmeta: SessionFrameMeta,
        force: bool,
    ) -> bool {
        self.enqueue_frame_inner(frame.as_ref(), Some(frame), ts, fmeta, force)
    }

    fn enqueue_frame_inner(
        self: &Arc<Self>,
        frame: &FrameBuf,
        shared_arc: Option<&Arc<FrameBuf>>,
        ts: f64,
        fmeta: SessionFrameMeta,
        force: bool,
    ) -> bool {
        if frame.data.is_empty() || frame.width == 0 || frame.height == 0 {
            return false;
        }
        // Fast gate/finished check before spending CPU on BGRA -> BGR24 conversion.
        {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.finished {
                return false;
            }
            if !force
                && let Some(prev) = st.last_ts
                && (ts - prev) < (1.0 / self.fps) * 0.9
            {
                return false;
            }
        }

        let bgr: Arc<FrameBuf> = if frame.format == PixelFormat::Bgr8
            && let Some(arc) = shared_arc
        {
            Arc::clone(arc)
        } else {
            match ensure_bgr24(frame) {
                Some(b) => Arc::new(b),
                None => return false,
            }
        };

        let reached_target = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.finished {
                return false;
            }
            if !force
                && let Some(prev) = st.last_ts
                && (ts - prev) < (1.0 / self.fps) * 0.9
            {
                return false;
            }
            if let Some((ew, eh)) = st.expected_dims {
                if bgr.width != ew || bgr.height != eh {
                    let (lock, _) = &*self.shared;
                    let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                    sh.dropped_resize += 1;
                    return false;
                }
            } else {
                st.expected_dims = Some((bgr.width, bgr.height));
            }
            {
                let (lock, _) = &*self.shared;
                let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                if sh.queue.len() >= sh.max_queue {
                    sh.dropped_queue += 1;
                    return false;
                }
            }
            if !st.worker_started
                && let Err(e) = self.start_worker_locked(&mut st, bgr.width, bgr.height)
            {
                tracing::warn!("[SessionRecorder] Failed to start worker: {e}");
                st.finished = true;
                return false;
            }
            if st.first_ts.is_none() {
                st.first_ts = Some(ts);
            }
            st.last_ts = Some(ts);
            let reached_target = self.seconds > 0.0
                && st.first_ts.is_some_and(|t0| (ts - t0) >= self.seconds);

            let (lock, cv) = &*self.shared;
            let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            sh.capture_paths_seen.insert(fmeta.capture_path.clone());
            if let (Some(mid), Some(wx), Some(wy)) = (
                fmeta.extra.get("map_id").and_then(Value::as_i64),
                fmeta.extra.get("world_x").and_then(Value::as_f64),
                fmeta.extra.get("world_y").and_then(Value::as_f64),
            ) && wx.is_finite()
                && wy.is_finite()
            {
                sh.visited_coords.push((mid as i32, wx, wy));
                if let Some(fid) = fmeta.extra.get("floor_id").and_then(Value::as_i64) {
                    sh.visited_floors.insert((mid as i32, fid as i32));
                }
            }

            let hotkeys = std::mem::take(&mut sh.pending_hotkeys);
            let events = std::mem::take(&mut sh.pending_events);
            let unrecorded_ticks = std::mem::take(&mut sh.pending_unrecorded_ticks);
            sh.tick_seq += 1;
            let tick_seq = sh.tick_seq;
            sh.frames += 1;
            let idx = sh.frames;
            let dropped = sh.dropped_queue + sh.dropped_resize;
            let meta = build_frame_meta_record(
                idx,
                ts,
                fmeta,
                dropped,
                hotkeys,
                events,
                tick_seq,
                unrecorded_ticks,
            );
            sh.queue.push_back(RecItem { idx, bgr, meta });
            cv.notify_one();
            reached_target
        };

        if reached_target {
            self.finish();
        }
        true
    }

    fn start_worker_locked(
        self: &Arc<Self>,
        st: &mut RecorderState,
        width: u32,
        height: u32,
    ) -> std::io::Result<()> {
        #[cfg(test)]
        if self.skip_worker_spawn {
            st.worker_started = true;
            return Ok(());
        }
        std::fs::create_dir_all(&self.out_dir)?;
        let video_path = self.out_dir.join("video.mkv");
        let child = spawn_ffmpeg_encoder(
            &self.ffmpeg_path,
            &video_path,
            width,
            height,
            self.fps,
            self.codec,
        )?;
        let meta_file = File::create(self.out_dir.join("meta.jsonl"))?;
        let writer = BufWriter::new(meta_file);
        let rec = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("SessionRecorder".to_owned())
            .spawn(move || rec.run_worker(child, writer, width, height))?;
        st.worker_started = true;
        st.worker = Some(handle);
        let dur_label = if self.seconds > 0.0 {
            format!("{:.0}s", self.seconds)
        } else {
            "manual-stop".to_owned()
        };
        tracing::info!(
            "[SessionRecorder] Recording {} @ {:.1} fps ({}x{}, {}) -> {}",
            dur_label,
            self.fps,
            width,
            height,
            self.codec.as_str(),
            self.out_dir.display()
        );
        Ok(())
    }

    fn run_worker(
        &self,
        mut child: Child,
        mut meta_writer: BufWriter<File>,
        width: u32,
        height: u32,
    ) {
        let mut ffmpeg_in = child.stdin.take();
        let (lock, cv) = &*self.shared;

        loop {
            let item = {
                let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                while sh.queue.is_empty() && !sh.stopped {
                    sh = cv.wait(sh).unwrap_or_else(|e| e.into_inner());
                }
                if let Some(next) = sh.queue.pop_front() {
                    Some(next)
                } else if sh.stopped {
                    None
                } else {
                    continue;
                }
            };

            let Some(item) = item else {
                break;
            };

            let video_ok = if let Some(ref mut stdin) = ffmpeg_in {
                write_packed_bgr24(stdin, &item.bgr).is_ok()
            } else {
                false
            };
            let meta_ok = serde_json::to_string(&item.meta)
                .map_err(std::io::Error::other)
                .and_then(|s| writeln!(meta_writer, "{s}"))
                .is_ok();
            if !video_ok || !meta_ok {
                let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                sh.write_errors += 1;
                if !video_ok {
                    tracing::warn!(
                        "[SessionRecorder] ffmpeg pipe write failed on frame {}",
                        item.idx
                    );
                }
            }
        }

        // Flush & wait for ffmpeg to finalize the Matroska container
        drop(ffmpeg_in);
        let ffmpeg_exit = child.wait().ok().and_then(|s| s.code());
        let _ = meta_writer.flush();

        let (
            frames,
            dropped_queue,
            dropped_resize,
            write_errors,
            capture_paths,
            visited_coords,
            visited_floors,
        ) = {
            let sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            (
                sh.frames,
                sh.dropped_queue,
                sh.dropped_resize,
                sh.write_errors,
                sh.capture_paths_seen.iter().cloned().collect::<Vec<_>>(),
                sh.visited_coords.clone(),
                sh.visited_floors.clone(),
            )
        };

        let video_size_bytes = std::fs::metadata(self.out_dir.join("video.mkv"))
            .map(|m| m.len())
            .unwrap_or(0);
        let assets_snap = finalize_session_artifacts(
            &self.out_dir,
            &self.tiles_dir,
            &self.ug_overlays_dir,
            &self.paimon_cache_path,
            &self.data_dir,
            &self.data_source,
            &self.maps_db,
            &visited_coords,
            &visited_floors,
            SessionInfoInputs {
                video_size_bytes,
                codec: self.codec,
                width,
                height,
                fps: self.fps,
                seconds_target: self.seconds,
                frames,
                dropped_queue,
                dropped_resize,
                write_errors,
                ffmpeg_exit_code: ffmpeg_exit,
                capture_paths_seen: capture_paths,
            },
        );
        tracing::info!(
            "[SessionRecorder] Done: {} frames ({} dropped, {:.1} MB, {} tiles / {} ug / {} data cached) -> {}",
            frames,
            dropped_queue + dropped_resize,
            video_size_bytes as f64 / (1024.0 * 1024.0),
            assets_snap.tiles_copied,
            assets_snap.ug_overlays_copied,
            assets_snap.datasets_copied,
            self.out_dir.display()
        );

        let cb = self
            .on_done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(cb) = cb {
            cb(self.out_dir.clone(), frames);
        }
    }

    pub fn finish(&self) {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.finished {
                return;
            }
            st.finished = true;
        }
        let (lock, cv) = &*self.shared;
        {
            let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            sh.stopped = true;
        }
        cv.notify_all();
    }

    pub fn join_timeout(&self, timeout: Duration) {
        self.finish();
        let handle = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker
            .take();
        let Some(handle) = handle else {
            return;
        };
        let deadline = std::time::Instant::now() + timeout;
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

fn format_event_entry(ts: f64, kind: &str, fields: Value) -> Value {
    let mut obj = match fields {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        other => {
            let mut m = Map::new();
            m.insert("value".to_owned(), other);
            m
        }
    };
    obj.insert(
        "ts".to_owned(),
        json!(((ts * 10_000.0).round()) / 10_000.0),
    );
    obj.insert("kind".to_owned(), Value::String(kind.to_owned()));
    Value::Object(obj)
}

#[allow(clippy::too_many_arguments)]
fn build_frame_meta_record(
    idx: usize,
    ts: f64,
    fmeta: SessionFrameMeta,
    dropped_so_far: usize,
    hotkeys: Vec<Value>,
    events: Vec<Value>,
    tick_seq: u64,
    unrecorded_ticks: BTreeMap<String, u64>,
) -> Value {
    let mut rec = Map::new();
    rec.insert("i".to_owned(), json!(idx));
    rec.insert(
        "ts".to_owned(),
        json!(((ts * 10_000.0).round()) / 10_000.0),
    );
    rec.insert("tick_seq".to_owned(), json!(tick_seq));
    let mut unrec_obj = Map::new();
    for (k, v) in unrecorded_ticks {
        if v > 0 {
            unrec_obj.insert(k, json!(v));
        }
    }
    rec.insert("unrecorded_ticks".to_owned(), Value::Object(unrec_obj));
    rec.insert("frame_w".to_owned(), json!(fmeta.client_rect.width));
    rec.insert("frame_h".to_owned(), json!(fmeta.client_rect.height));
    rec.insert(
        "client_rect".to_owned(),
        json!([
            fmeta.client_rect.x,
            fmeta.client_rect.y,
            fmeta.client_rect.width,
            fmeta.client_rect.height
        ]),
    );
    rec.insert(
        "cursor_client_pos".to_owned(),
        match fmeta.cursor_client_pos {
            Some((cx, cy)) => json!([
                ((cx * 10.0).round()) / 10.0,
                ((cy * 10.0).round()) / 10.0
            ]),
            None => Value::Null,
        },
    );
    rec.insert("capture_path".to_owned(), Value::String(fmeta.capture_path));
    rec.insert("dropped_so_far".to_owned(), json!(dropped_so_far));
    if !hotkeys.is_empty() {
        rec.insert("hotkeys".to_owned(), Value::Array(hotkeys));
    }
    if !events.is_empty() {
        rec.insert("events".to_owned(), Value::Array(events));
    }
    for (k, v) in fmeta.extra {
        rec.insert(k, v);
    }
    Value::Object(rec)
}

fn ensure_bgr24(frame: &FrameBuf) -> Option<FrameBuf> {
    match frame.format {
        PixelFormat::Bgr8 => Some(frame.clone()),
        PixelFormat::Bgra8 => {
            let w = frame.width as usize;
            let h = frame.height as usize;
            let mut out = FrameBuf::new_zeroed(frame.width, frame.height, PixelFormat::Bgr8);
            for y in 0..h {
                let s_row = &frame.data[y * frame.stride..];
                let d_row = &mut out.data[y * out.stride..];
                for x in 0..w {
                    let s = x * 4;
                    let d = x * 3;
                    d_row[d] = s_row[s];
                    d_row[d + 1] = s_row[s + 1];
                    d_row[d + 2] = s_row[s + 2];
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn write_packed_bgr24(writer: &mut impl Write, frame: &FrameBuf) -> std::io::Result<()> {
    let row_bytes = frame.width as usize * 3;
    if frame.stride == row_bytes {
        writer.write_all(&frame.data[..row_bytes * frame.height as usize])
    } else {
        for y in 0..frame.height as usize {
            let start = y * frame.stride;
            writer.write_all(&frame.data[start..start + row_bytes])?;
        }
        Ok(())
    }
}

fn spawn_ffmpeg_encoder(
    ffmpeg_abs: &Path,
    video_path: &Path,
    width: u32,
    height: u32,
    fps: f64,
    codec: SessionCodec,
) -> std::io::Result<Child> {
    let size_arg = format!("{width}x{height}");
    let fps_arg = format!("{fps:.3}");
    let mut cmd = Command::new(ffmpeg_abs);
    cmd.args([
        "-y",
        "-v",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgr24",
        "-s",
        &size_arg,
        "-r",
        &fps_arg,
        "-i",
        "pipe:0",
    ]);
    match codec {
        SessionCodec::Libx264Rgb => {
            cmd.args([
                "-c:v",
                "libx264rgb",
                "-qp",
                "0",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "bgr24",
            ]);
        }
        SessionCodec::Ffv1 => {
            cmd.args(["-c:v", "ffv1", "-level", "3", "-g", "1", "-pix_fmt", "bgr0"]);
        }
    }
    #[cfg(windows)]
    {
        // Keep ffmpeg's console from popping up on every Alt+F8 take.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg(video_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
}

#[derive(Debug, Clone, Copy, Default)]
struct AssetsSnapshotStats {
    tiles_copied: usize,
    ug_overlays_copied: usize,
    paimon_copied: bool,
    datasets_copied: usize,
}

struct SessionInfoInputs {
    video_size_bytes: u64,
    codec: SessionCodec,
    width: u32,
    height: u32,
    fps: f64,
    seconds_target: f64,
    frames: usize,
    dropped_queue: usize,
    dropped_resize: usize,
    write_errors: usize,
    ffmpeg_exit_code: Option<i32>,
    capture_paths_seen: Vec<String>,
}

fn build_session_provenance(data_dir: &Path, data_source: &str) -> SessionProvenance {
    SessionProvenance {
        app_version: BUILD_APP_VERSION.to_owned(),
        git_commit: BUILD_GIT_COMMIT.to_owned(),
        data_source: sanitize_data_source_label(data_source),
        datasets: fingerprint_data_dir(data_dir),
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_session_artifacts(
    out_dir: &Path,
    src_tiles_dir: &Path,
    src_ug_dir: &Path,
    src_paimon_path: &Path,
    src_data_dir: &Path,
    data_source: &str,
    maps_db: &MultimapDb,
    visited_coords: &[(i32, f64, f64)],
    visited_floors: &HashSet<(i32, i32)>,
    inputs: SessionInfoInputs,
) -> AssetsSnapshotStats {
    // Mandatory hermetic assets_cache/ snapshot for offline replay parity
    let assets_snap = snapshot_session_assets(
        &out_dir.join("assets_cache"),
        src_tiles_dir,
        src_ug_dir,
        src_paimon_path,
        src_data_dir,
        maps_db,
        visited_coords,
        visited_floors,
    );
    let provenance = build_session_provenance(src_data_dir, data_source);

    let info = json!({
        "schema_version": 1,
        "video_file": "video.mkv",
        "video_size_bytes": inputs.video_size_bytes,
        "codec": inputs.codec.as_str(),
        "width": inputs.width,
        "height": inputs.height,
        "fps": inputs.fps,
        "seconds_target": inputs.seconds_target,
        "frames": inputs.frames,
        "dropped_queue": inputs.dropped_queue,
        "dropped_resize": inputs.dropped_resize,
        "dropped_total": inputs.dropped_queue + inputs.dropped_resize,
        "write_errors": inputs.write_errors,
        "ffmpeg_exit_code": inputs.ffmpeg_exit_code,
        "capture_paths_seen": inputs.capture_paths_seen,
        "assets_cache": {
            "tiles_copied": assets_snap.tiles_copied,
            "ug_overlays_copied": assets_snap.ug_overlays_copied,
            "paimon_copied": assets_snap.paimon_copied,
        },
        "provenance": provenance,
    });
    let _ = write_json_atomic(&out_dir.join("info.json"), &info);
    assets_snap
}

/// Mandatorily copies all cached HD tiles, underground overlays, `paimon_tpl.npz`, and live
/// dataset files relevant to the session into `<session>/assets_cache/`.
#[allow(clippy::too_many_arguments)]
fn snapshot_session_assets(
    assets_cache_dir: &Path,
    src_tiles_dir: &Path,
    src_ug_dir: &Path,
    src_paimon_path: &Path,
    src_data_dir: &Path,
    maps_db: &MultimapDb,
    visited_coords: &[(i32, f64, f64)],
    visited_floors: &HashSet<(i32, i32)>,
) -> AssetsSnapshotStats {
    let dst_tiles = assets_cache_dir.join("tiles");
    let dst_ug = assets_cache_dir.join("ug_overlays");
    let _ = std::fs::create_dir_all(&dst_tiles);
    let _ = std::fs::create_dir_all(&dst_ug);

    let mut stats = AssetsSnapshotStats::default();
    let mut tile_keys: HashSet<(i32, i32, i32, i32)> = HashSet::new();
    let mut map_ids_seen: HashSet<i32> = HashSet::new();

    for &(mid, wx, wy) in visited_coords {
        map_ids_seen.insert(mid);
        if let Some(d2) = maps_db.map(mid).and_then(|m| m.detail_v2.as_ref()) {
            let [ox, oy] = d2.origin;
            for z in -2..=0 {
                let scale = 2.0_f64.powi(z);
                let cx = ((wx + ox) * scale / 256.0).floor() as i32;
                let cy = ((wy + oy) * scale / 256.0).floor() as i32;
                for dy in -2..=2 {
                    for dx in -2..=2 {
                        let tx = cx + dx;
                        let ty = cy + dy;
                        if tx >= 0 && ty >= 0 {
                            tile_keys.insert((mid, z, tx, ty));
                        }
                    }
                }
            }
        }
    }

    for (mid, z, tx, ty) in tile_keys {
        for ext in ["webp", "png"] {
            let rel = PathBuf::from(mid.to_string())
                .join(z.to_string())
                .join(format!("{tx}_{ty}.{ext}"));
            let src = src_tiles_dir.join(&rel);
            if src.is_file() {
                let dst = dst_tiles.join(&rel);
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::copy(&src, &dst).is_ok() {
                    stats.tiles_copied += 1;
                }
            }
        }
    }

    // Copy any cached underground overlays matching visited floors or overlapping visited coordinates.
    let mut ug_filenames: HashSet<String> = HashSet::new();
    for &(mid, fid) in visited_floors {
        for rec in maps_db.floor_overlays_for_map(mid) {
            if rec.floor_id == fid {
                ug_filenames.insert(format!("{mid}_g{}_f{fid}.png", rec.group_id));
            }
        }
    }
    for &(mid, wx, wy) in visited_coords {
        for rec in maps_db.floor_overlays_for_map(mid) {
            if (rec.lx - 180.0) <= wx
                && wx <= (rec.rx + 180.0)
                && (rec.ly - 180.0) <= wy
                && wy <= (rec.ry + 180.0)
            {
                ug_filenames.insert(format!("{mid}_g{}_f{}.png", rec.group_id, rec.floor_id));
            }
        }
    }
    // Also copy any existing UG overlays for seen maps if visited_coords was empty (e.g. M-Map-only session)
    if visited_coords.is_empty() && src_ug_dir.is_dir() {
        for mid in &map_ids_seen {
            let prefix = format!("{mid}_g");
            if let Ok(entries) = std::fs::read_dir(src_ug_dir) {
                for ent in entries.flatten() {
                    if let Some(name) = ent.file_name().to_str()
                        && name.starts_with(&prefix)
                        && name.ends_with(".png")
                    {
                        ug_filenames.insert(name.to_owned());
                    }
                }
            }
        }
    }
    for fname in ug_filenames {
        let src = src_ug_dir.join(&fname);
        if src.is_file() && std::fs::copy(&src, dst_ug.join(&fname)).is_ok() {
            stats.ug_overlays_copied += 1;
        }
    }

    if src_paimon_path.is_file()
        && std::fs::copy(src_paimon_path, assets_cache_dir.join("paimon_tpl.npz")).is_ok()
    {
        stats.paimon_copied = true;
    }

    let dst_data = assets_cache_dir.join("data");
    let _ = std::fs::create_dir_all(&dst_data);
    for fname in DATASET_FILENAMES {
        let src = src_data_dir.join(fname);
        if src.is_file() && std::fs::copy(&src, dst_data.join(fname)).is_ok() {
            stats.datasets_copied += 1;
        }
    }

    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::multimap::load_multimap_db;

    #[test]
    fn validate_ffmpeg_rejects_missing_relative_and_nonexistent_paths() {
        assert!(validate_ffmpeg_path(None).is_err());
        assert!(validate_ffmpeg_path(Some(Path::new("ffmpeg.exe"))).is_err());
        assert!(validate_ffmpeg_path(Some(Path::new("./ffmpeg.exe"))).is_err());
        assert!(
            validate_ffmpeg_path(Some(Path::new(
                "C:\\nonexistent_dir_12345\\ffmpeg.exe"
            )))
            .is_err()
        );
        // Existing absolute file passes
        let exe = std::env::current_exe().unwrap();
        assert_eq!(validate_ffmpeg_path(Some(&exe)).unwrap(), exe);
    }

    #[test]
    fn codec_parser_supports_lossless_rgb_codecs_only() {
        assert_eq!(SessionCodec::parse("libx264rgb").unwrap(), SessionCodec::Libx264Rgb);
        assert_eq!(SessionCodec::parse("FFV1").unwrap(), SessionCodec::Ffv1);
        assert!(SessionCodec::parse("libx264_lossy").is_err());
    }

    #[test]
    fn snapshot_session_assets_copies_tiles_ug_overlays_and_paimon_cache() {
        let root = std::env::temp_dir().join(format!(
            "gh_sess_assets_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src_tiles = root.join("src_tiles");
        let src_ug = root.join("src_ug");
        let dst_cache = root.join("out").join("assets_cache");
        std::fs::create_dir_all(src_tiles.join("2").join("0")).unwrap();
        std::fs::create_dir_all(&src_ug).unwrap();

        // Write minimal multimap DB with origin [24207, 16411] so world (0,0) maps to tile (94, 64) at z=0
        let db_path = root.join("multimap.json");
        write_json_atomic(
            &db_path,
            &json!({
                "version": "4.0.0",
                "maps": {
                    "2": {
                        "id": 2,
                        "name": "Teyvat",
                        "detail_v2": {
                            "total_size": [36864, 32768],
                            "padding": [7625, 6881],
                            "origin": [24207, 16411],
                            "min_zoom": -4,
                            "max_zoom": 0,
                            "original_map_size": [21613, 19006]
                        },
                        "point_groups": [
                            {
                                "id": 10,
                                "map_id": 2,
                                "name": "Test Cave",
                                "floors": [
                                    {
                                        "id": 77,
                                        "floor_name": "B1",
                                        "floor_name_short": "B1",
                                        "index": 0,
                                        "overlay": {
                                            "url": "https://example.invalid/2_g10_f77.png",
                                            "l_x": -100.0,
                                            "l_y": -100.0,
                                            "r_x": 100.0,
                                            "r_y": 100.0
                                        }
                                    }
                                ]
                            }
                        ]
                    }
                }
            }),
        )
        .unwrap();
        let db = load_multimap_db(&db_path).unwrap();

        std::fs::write(src_tiles.join("2").join("0").join("94_64.webp"), b"RIFF1234WEBP").unwrap();
        std::fs::write(src_ug.join("2_g10_f77.png"), b"PNGDATA").unwrap();
        let paimon = root.join("paimon_tpl.npz");
        std::fs::write(&paimon, b"NPZDATA").unwrap();

        let src_data = root.join("src_data");
        std::fs::create_dir_all(&src_data).unwrap();
        std::fs::write(src_data.join(DATASET_FILENAMES[0]), b"{\"maps\":{}}").unwrap();
        std::fs::write(src_data.join(DATASET_FILENAMES[1]), b"TERRAIN").unwrap();

        let mut floors = HashSet::new();
        floors.insert((2, 77));
        let st = snapshot_session_assets(
            &dst_cache,
            &src_tiles,
            &src_ug,
            &paimon,
            &src_data,
            &db,
            &[(2, 0.0, 0.0)],
            &floors,
        );
        assert_eq!(st.tiles_copied, 1);
        assert_eq!(st.ug_overlays_copied, 1);
        assert!(st.paimon_copied);
        assert_eq!(st.datasets_copied, 2);
        assert!(dst_cache.join("tiles").join("2").join("0").join("94_64.webp").is_file());
        assert!(dst_cache.join("ug_overlays").join("2_g10_f77.png").is_file());
        assert!(dst_cache.join("paimon_tpl.npz").is_file());
        assert!(dst_cache.join("data").join(DATASET_FILENAMES[0]).is_file());
        assert!(dst_cache.join("data").join(DATASET_FILENAMES[1]).is_file());
        assert!(!dst_cache.join("data").join(DATASET_FILENAMES[2]).exists());
        assert!(!dst_cache.join("data").join(DATASET_FILENAMES[3]).exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finalize_session_artifacts_copies_datasets_and_writes_provenance_without_paths() {
        let root = std::env::temp_dir().join(format!(
            "gh_sess_prov_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let out_dir = root.join("sess_prov_test");
        let src_data = root.join("live_data");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::create_dir_all(&src_data).unwrap();

        let mm_bytes = br#"{"version":"4.0.0","maps":{}}"#;
        let terrain_bytes = b"TERRAIN_BYTES";
        let cave_bytes = b"CAVE_BYTES";
        std::fs::write(src_data.join(DATASET_FILENAMES[0]), mm_bytes).unwrap();
        std::fs::write(src_data.join(DATASET_FILENAMES[1]), terrain_bytes).unwrap();
        std::fs::write(src_data.join(DATASET_FILENAMES[2]), cave_bytes).unwrap();
        // DATASET_FILENAMES[3] intentionally missing (matching installed dataset layout)

        let db = load_multimap_db(&src_data.join(DATASET_FILENAMES[0])).unwrap();
        let st = finalize_session_artifacts(
            &out_dir,
            &root.join("tiles"),
            &root.join("ug"),
            &root.join("paimon_tpl.npz"),
            &src_data,
            "installed",
            &db,
            &[],
            &HashSet::new(),
            SessionInfoInputs {
                video_size_bytes: 1234,
                codec: SessionCodec::Libx264Rgb,
                width: 1280,
                height: 720,
                fps: 20.0,
                seconds_target: 0.0,
                frames: 10,
                dropped_queue: 0,
                dropped_resize: 0,
                write_errors: 0,
                ffmpeg_exit_code: Some(0),
                capture_paths_seen: vec!["gdi".to_owned()],
            },
        );
        assert_eq!(st.datasets_copied, 3);
        assert_eq!(
            std::fs::read(out_dir.join("assets_cache").join("data").join(DATASET_FILENAMES[0]))
                .unwrap(),
            mm_bytes
        );
        assert_eq!(
            std::fs::read(out_dir.join("assets_cache").join("data").join(DATASET_FILENAMES[1]))
                .unwrap(),
            terrain_bytes
        );
        assert_eq!(
            std::fs::read(out_dir.join("assets_cache").join("data").join(DATASET_FILENAMES[2]))
                .unwrap(),
            cave_bytes
        );
        assert!(
            !out_dir
                .join("assets_cache")
                .join("data")
                .join(DATASET_FILENAMES[3])
                .exists()
        );

        let info: Value = serde_json::from_str(
            &std::fs::read_to_string(out_dir.join("info.json")).unwrap(),
        )
        .unwrap();
        // Existing keys unchanged
        assert_eq!(info["schema_version"], json!(1));
        assert_eq!(info["frames"], json!(10));
        assert_eq!(info["codec"], json!("libx264rgb"));
        assert_eq!(info["assets_cache"]["tiles_copied"], json!(0));

        // Provenance stamped accurately
        let prov = &info["provenance"];
        assert_eq!(prov["app_version"], json!(BUILD_APP_VERSION));
        assert_eq!(prov["git_commit"], json!(BUILD_GIT_COMMIT));
        assert_eq!(prov["data_source"], json!("installed"));
        let expected_fp = fingerprint_data_dir(&src_data);
        assert_eq!(
            prov["datasets"],
            serde_json::to_value(&expected_fp).unwrap()
        );
        assert!(prov["datasets"][DATASET_FILENAMES[3]].is_null());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn record_event_queues_into_next_frame_meta_and_is_noop_when_inactive() {
        let root = std::env::temp_dir().join(format!(
            "gh_sess_events_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let db_path = root.join("multimap.json");
        write_json_atomic(&db_path, &json!({"version": "4.0.0", "maps": {}})).unwrap();
        let db = Arc::new(load_multimap_db(&db_path).unwrap());

        let rec = SessionRecorder::new(
            SessionRecorderConfig {
                seconds: 0.0,
                fps: DEFAULT_SESSION_FPS,
                codec: SessionCodec::Libx264Rgb,
                ffmpeg_path: std::env::current_exe().unwrap(),
                out_root: root.join("out"),
                tiles_dir: root.join("tiles"),
                ug_overlays_dir: root.join("ug"),
                paimon_cache_path: root.join("paimon.npz"),
                data_dir: root.clone(),
                data_source: "dev".to_owned(),
                maps_db: db,
                max_queue: 16,
            },
            None,
        )
        .unwrap();

        assert!(rec.active());
        rec.record_hotkey_event(12.34567, "sync");
        rec.record_event(
            12.50004,
            "sync_broadcast",
            json!({"seq": 7u64, "msg_type": "SYNC_POSITION_ONLY", "map_id": 40}),
        );
        rec.record_event(
            13.25,
            "web_feedback",
            json!({"event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 7u64}),
        );

        let hotkeys = rec.take_pending_hotkeys_for_test();
        let events = rec.take_pending_events_for_test();
        assert_eq!(hotkeys.len(), 1);
        assert_eq!(events.len(), 2);

        let frame_meta = build_frame_meta_record(
            1,
            13.3,
            SessionFrameMeta {
                client_rect: RectI32 {
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                },
                cursor_client_pos: None,
                capture_path: "gdi".to_owned(),
                extra: Map::new(),
            },
            0,
            hotkeys,
            events,
            1,
            BTreeMap::new(),
        );

        // hotkeys preserved intact
        assert_eq!(frame_meta["hotkeys"][0]["action"], "sync");
        assert_eq!(frame_meta["hotkeys"][0]["ts"], 12.3457);

        // events attached with ts, kind, and fields
        let evs = frame_meta["events"].as_array().expect("events array");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["ts"], 12.5);
        assert_eq!(evs[0]["kind"], "sync_broadcast");
        assert_eq!(evs[0]["seq"], 7);
        assert_eq!(evs[0]["msg_type"], "SYNC_POSITION_ONLY");
        assert_eq!(evs[0]["map_id"], 40);
        assert_eq!(evs[1]["ts"], 13.25);
        assert_eq!(evs[1]["kind"], "web_feedback");
        assert_eq!(evs[1]["event_type"], "SYNC_POSITION_ONLY");
        assert_eq!(evs[1]["status"], "synced");
        assert_eq!(evs[1]["seq"], 7);

        // Next frame with no queued events omits `events` key
        let frame_meta_empty = build_frame_meta_record(
            2,
            13.35,
            SessionFrameMeta {
                client_rect: RectI32 {
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                },
                cursor_client_pos: None,
                capture_path: "gdi".to_owned(),
                extra: Map::new(),
            },
            0,
            rec.take_pending_hotkeys_for_test(),
            rec.take_pending_events_for_test(),
            2,
            BTreeMap::new(),
        );
        assert!(frame_meta_empty.get("events").is_none());
        assert!(frame_meta_empty.get("hotkeys").is_none());

        // Once finished/inactive, record_event is a strict no-op
        rec.finish();
        assert!(!rec.active());
        rec.record_event(
            14.0,
            "web_feedback",
            json!({"event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 8u64}),
        );
        assert!(rec.take_pending_events_for_test().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    fn mk_test_fmeta(w: i32, h: i32, mode: &str) -> SessionFrameMeta {
        let mut extra = Map::new();
        extra.insert("mode".to_owned(), json!(mode));
        SessionFrameMeta {
            client_rect: RectI32 {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            cursor_client_pos: None,
            capture_path: "gdi".to_owned(),
            extra,
        }
    }

    #[test]
    fn add_frame_forced_bypasses_gate_while_non_forced_rejects_within_45ms() {
        let root = std::env::temp_dir().join(format!("gh_sess_force_{}", std::process::id()));
        let rec = SessionRecorder::new_dry_run_for_test(&root, 20.0, 16);
        let frame = FrameBuf::new_zeroed(16, 16, PixelFormat::Bgr8);

        // First frame always accepted
        assert!(rec.add_frame(&frame, 1.000, mk_test_fmeta(16, 16, "minimap"), false));
        // Non-forced within 45 ms (< 0.9 / 20 = 0.045 s) is rejected
        assert!(!rec.add_frame(&frame, 1.020, mk_test_fmeta(16, 16, "minimap"), false));
        assert!(!rec.add_frame(&frame, 1.044, mk_test_fmeta(16, 16, "map_m"), false));
        // Forced within 45 ms is accepted (e.g. Closed -> Settling at +20 ms, Settling -> SyncedIdle at +25 ms)
        assert!(rec.add_frame(&frame, 1.020, mk_test_fmeta(16, 16, "map_m"), true));
        assert!(rec.add_frame(&frame, 1.045, mk_test_fmeta(16, 16, "map_m"), true));
        // Subsequent non-forced within 45 ms of the forced frame (1.045) is still rejected
        assert!(!rec.add_frame(&frame, 1.070, mk_test_fmeta(16, 16, "map_m"), false));
        // Once >= 45 ms elapsed after 1.045 (1.095), non-forced is accepted again
        assert!(rec.add_frame(&frame, 1.095, mk_test_fmeta(16, 16, "map_m"), false));

        let queued = rec.drain_queued_items_for_test();
        assert_eq!(queued.len(), 4);
        for (k, (idx, meta)) in queued.iter().enumerate() {
            assert_eq!(*idx, k + 1);
            assert_eq!(meta["i"], json!(k + 1));
        }
        assert_eq!(queued[0].1["ts"], json!(1.0));
        assert_eq!(queued[1].1["ts"], json!(1.02));
        assert_eq!(queued[2].1["ts"], json!(1.045));
        assert_eq!(queued[3].1["ts"], json!(1.095));
    }

    #[test]
    fn unrecorded_ticks_land_in_next_kept_frame_and_reset_with_monotonic_tick_seq() {
        let root = std::env::temp_dir().join(format!("gh_sess_unrec_{}", std::process::id()));
        let rec = SessionRecorder::new_dry_run_for_test(&root, 20.0, 16);
        let frame = Arc::new(FrameBuf::new_zeroed(16, 16, PixelFormat::Bgr8));

        // 1 unrecorded tick before first kept frame
        rec.record_unrecorded_tick("idle_closed");
        assert!(rec.add_frame_arc(&frame, 1.000, mk_test_fmeta(16, 16, "minimap"), false));

        // 3 unrecorded ticks before next kept frame (2 minimap_fast + 1 map_m)
        rec.record_unrecorded_tick("minimap_fast");
        rec.record_unrecorded_tick("minimap_fast");
        rec.record_unrecorded_tick("map_m");
        assert!(rec.add_frame_arc(&frame, 1.025, mk_test_fmeta(16, 16, "map_m"), true));

        // 0 unrecorded ticks before third kept frame -> empty object (reset verified)
        assert!(rec.add_frame_arc(&frame, 1.050, mk_test_fmeta(16, 16, "map_m"), true));

        // Inactive recorder ignores record_unrecorded_tick
        rec.finish();
        rec.record_unrecorded_tick("minimap");

        let queued = rec.drain_queued_items_for_test();
        assert_eq!(queued.len(), 3);

        assert_eq!(queued[0].1["i"], json!(1));
        assert_eq!(queued[0].1["tick_seq"], json!(2));
        assert_eq!(queued[0].1["unrecorded_ticks"], json!({"idle_closed": 1}));

        assert_eq!(queued[1].1["i"], json!(2));
        assert_eq!(queued[1].1["tick_seq"], json!(6));
        assert_eq!(
            queued[1].1["unrecorded_ticks"],
            json!({"map_m": 1, "minimap_fast": 2})
        );

        assert_eq!(queued[2].1["i"], json!(3));
        assert_eq!(queued[2].1["tick_seq"], json!(7));
        assert_eq!(queued[2].1["unrecorded_ticks"], json!({}));
    }

    #[test]
    fn meta_frame_index_stays_aligned_when_queue_overflows_or_frame_resizes() {
        let root = std::env::temp_dir().join(format!("gh_sess_align_{}", std::process::id()));
        // Tiny queue (capacity 2) with dry-run (no consumer popping)
        let rec = SessionRecorder::new_dry_run_for_test(&root, 20.0, 2);
        let f16 = FrameBuf::new_zeroed(16, 16, PixelFormat::Bgr8);
        let f32 = FrameBuf::new_zeroed(32, 32, PixelFormat::Bgr8);

        assert!(rec.add_frame(&f16, 1.00, mk_test_fmeta(16, 16, "minimap"), false));
        // Resize mismatch -> rejected without advancing `i`
        assert!(!rec.add_frame(&f32, 1.10, mk_test_fmeta(32, 32, "minimap"), false));
        rec.record_unrecorded_tick("minimap");
        assert!(rec.add_frame(&f16, 1.20, mk_test_fmeta(16, 16, "minimap"), false));
        // Queue is now full (len == 2): next frame must fail without advancing `i`
        assert!(!rec.add_frame(&f16, 1.30, mk_test_fmeta(16, 16, "map_m"), true));
        rec.record_unrecorded_tick("map_m_settling");

        let first_batch = rec.drain_queued_items_for_test();
        assert_eq!(first_batch.len(), 2);
        assert_eq!(first_batch[0].0, 1);
        assert_eq!(first_batch[0].1["i"], json!(1));
        assert_eq!(first_batch[1].0, 2);
        assert_eq!(first_batch[1].1["i"], json!(2));
        assert_eq!(first_batch[1].1["unrecorded_ticks"], json!({"minimap": 1}));

        // After draining queue, next frame gets contiguous index 3 (NOT 4) and carries the overflow count
        assert!(rec.add_frame(&f16, 1.40, mk_test_fmeta(16, 16, "map_m"), false));
        let next_batch = rec.drain_queued_items_for_test();
        assert_eq!(next_batch.len(), 1);
        assert_eq!(next_batch[0].0, 3);
        assert_eq!(next_batch[0].1["i"], json!(3));
        assert_eq!(
            next_batch[0].1["unrecorded_ticks"],
            json!({"map_m_settling": 1})
        );
    }

    #[test]
    fn forced_frame_cost_1440p_zero_copy_bgr8_and_bgra8_conversion() {
        let root = std::env::temp_dir().join(format!("gh_sess_cost_{}", std::process::id()));
        let rec_bgr = SessionRecorder::new_dry_run_for_test(&root, 20.0, 32);
        let bgr_1440p = Arc::new(FrameBuf::new_zeroed(2560, 1440, PixelFormat::Bgr8));
        let bgra_1440p = FrameBuf::new_zeroed(2560, 1440, PixelFormat::Bgra8);

        // Warm up once
        assert!(rec_bgr.add_frame_arc(&bgr_1440p, 1.0, mk_test_fmeta(2560, 1440, "map_m"), true));
        let _ = rec_bgr.drain_queued_items_for_test();

        let iters = 10usize;
        let t0 = std::time::Instant::now();
        for k in 0..iters {
            assert!(rec_bgr.add_frame_arc(
                &bgr_1440p,
                2.0 + k as f64 * 0.025,
                mk_test_fmeta(2560, 1440, "map_m"),
                true,
            ));
        }
        let bgr_arc_us = t0.elapsed().as_secs_f64() * 1e6 / iters as f64;
        // Verify zero-copy: queued RecItem shares the exact Arc<FrameBuf> allocation.
        {
            let (lock, _) = &*rec_bgr.shared;
            let sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(sh.queue.len(), iters);
            assert!(Arc::ptr_eq(&sh.queue[0].bgr, &bgr_1440p));
        }
        let _ = rec_bgr.drain_queued_items_for_test();

        let rec_bgra = SessionRecorder::new_dry_run_for_test(&root, 20.0, 32);
        let t1 = std::time::Instant::now();
        for k in 0..iters {
            assert!(rec_bgra.add_frame(
                &bgra_1440p,
                2.0 + k as f64 * 0.025,
                mk_test_fmeta(2560, 1440, "map_m"),
                true,
            ));
        }
        let bgra_ms = t1.elapsed().as_secs_f64() * 1e3 / iters as f64;
        let _ = rec_bgra.drain_queued_items_for_test();

        eprintln!(
            "[forced_frame_cost_1440p] add_frame_arc(Bgr8 zero-copy)={bgr_arc_us:.1} us/frame, add_frame(Bgra8->Bgr8)={bgra_ms:.2} ms/frame"
        );
    }
}
