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
use gh_core::atomic_io::write_json_atomic;
use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::multimap::MultimapDb;
use gh_core::{CoreError, CoreResult};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet, VecDeque};
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
    bgr: FrameBuf,
    meta: Value,
}

struct RecorderShared {
    queue: VecDeque<RecItem>,
    max_queue: usize,
    stopped: bool,
    frames: usize,
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
    maps_db: Arc<MultimapDb>,
    on_done: Mutex<Option<SessionDoneCallback>>,
    shared: Arc<(Mutex<RecorderShared>, Condvar)>,
    state: Mutex<RecorderState>,
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
            maps_db: cfg.maps_db,
            on_done: Mutex::new(on_done),
            shared: Arc::new((
                Mutex::new(RecorderShared {
                    queue: VecDeque::new(),
                    max_queue: cfg.max_queue.clamp(8, DEFAULT_SESSION_QUEUE * 4),
                    stopped: false,
                    frames: 0,
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
        }))
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

    pub fn add_frame(self: &Arc<Self>, frame: &FrameBuf, ts: f64, fmeta: SessionFrameMeta) {
        if frame.data.is_empty() || frame.width == 0 || frame.height == 0 {
            return;
        }
        let bgr = match ensure_bgr24(frame) {
            Some(b) => b,
            None => return,
        };

        let (idx, reached_target, dropped_so_far, hotkeys, events) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.finished {
                return;
            }
            if let Some(prev) = st.last_ts
                && (ts - prev) < (1.0 / self.fps) * 0.9
            {
                return;
            }
            if let Some((ew, eh)) = st.expected_dims {
                if bgr.width != ew || bgr.height != eh {
                    let (lock, _) = &*self.shared;
                    let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                    sh.dropped_resize += 1;
                    return;
                }
            } else {
                st.expected_dims = Some((bgr.width, bgr.height));
            }
            if !st.worker_started
                && let Err(e) = self.start_worker_locked(&mut st, bgr.width, bgr.height)
            {
                tracing::warn!("[SessionRecorder] Failed to start worker: {e}");
                st.finished = true;
                return;
            }
            if st.first_ts.is_none() {
                st.first_ts = Some(ts);
            }
            st.last_ts = Some(ts);
            let reached_target = self.seconds > 0.0
                && st.first_ts.is_some_and(|t0| (ts - t0) >= self.seconds);

            let (lock, _) = &*self.shared;
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
            sh.frames += 1;
            let idx = sh.frames;
            let dropped = sh.dropped_queue + sh.dropped_resize;
            (idx, reached_target, dropped, hotkeys, events)
        };

        let meta = build_frame_meta_record(idx, ts, fmeta, dropped_so_far, hotkeys, events);

        {
            let (lock, cv) = &*self.shared;
            let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            if sh.queue.len() >= sh.max_queue {
                sh.dropped_queue += 1;
            } else {
                sh.queue.push_back(RecItem { idx, bgr, meta });
                cv.notify_one();
            }
        }

        if reached_target {
            self.finish();
        }
    }

    fn start_worker_locked(
        self: &Arc<Self>,
        st: &mut RecorderState,
        width: u32,
        height: u32,
    ) -> std::io::Result<()> {
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

        // Mandatory hermetic assets_cache/ snapshot for offline replay parity
        let assets_snap = snapshot_session_assets(
            &self.out_dir.join("assets_cache"),
            &self.tiles_dir,
            &self.ug_overlays_dir,
            &self.paimon_cache_path,
            &self.maps_db,
            &visited_coords,
            &visited_floors,
        );

        let video_size_bytes = std::fs::metadata(self.out_dir.join("video.mkv"))
            .map(|m| m.len())
            .unwrap_or(0);
        let info = json!({
            "schema_version": 1,
            "video_file": "video.mkv",
            "video_size_bytes": video_size_bytes,
            "codec": self.codec.as_str(),
            "width": width,
            "height": height,
            "fps": self.fps,
            "seconds_target": self.seconds,
            "frames": frames,
            "dropped_queue": dropped_queue,
            "dropped_resize": dropped_resize,
            "dropped_total": dropped_queue + dropped_resize,
            "write_errors": write_errors,
            "ffmpeg_exit_code": ffmpeg_exit,
            "capture_paths_seen": capture_paths,
            "assets_cache": {
                "tiles_copied": assets_snap.tiles_copied,
                "ug_overlays_copied": assets_snap.ug_overlays_copied,
                "paimon_copied": assets_snap.paimon_copied,
            },
        });
        let _ = write_json_atomic(&self.out_dir.join("info.json"), &info);
        tracing::info!(
            "[SessionRecorder] Done: {} frames ({} dropped, {:.1} MB, {} tiles / {} ug cached) -> {}",
            frames,
            dropped_queue + dropped_resize,
            video_size_bytes as f64 / (1024.0 * 1024.0),
            assets_snap.tiles_copied,
            assets_snap.ug_overlays_copied,
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

fn build_frame_meta_record(
    idx: usize,
    ts: f64,
    fmeta: SessionFrameMeta,
    dropped_so_far: usize,
    hotkeys: Vec<Value>,
    events: Vec<Value>,
) -> Value {
    let mut rec = Map::new();
    rec.insert("i".to_owned(), json!(idx));
    rec.insert(
        "ts".to_owned(),
        json!(((ts * 10_000.0).round()) / 10_000.0),
    );
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
}

/// Mandatorily copies all cached HD tiles & underground overlays relevant to the visited trajectory
/// (plus `paimon_tpl.npz` if present) into `<session>/assets_cache/`.
fn snapshot_session_assets(
    assets_cache_dir: &Path,
    src_tiles_dir: &Path,
    src_ug_dir: &Path,
    src_paimon_path: &Path,
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

        let mut floors = HashSet::new();
        floors.insert((2, 77));
        let st = snapshot_session_assets(
            &dst_cache,
            &src_tiles,
            &src_ug,
            &paimon,
            &db,
            &[(2, 0.0, 0.0)],
            &floors,
        );
        assert_eq!(st.tiles_copied, 1);
        assert_eq!(st.ug_overlays_copied, 1);
        assert!(st.paimon_copied);
        assert!(dst_cache.join("tiles").join("2").join("0").join("94_64.webp").is_file());
        assert!(dst_cache.join("ug_overlays").join("2_g10_f77.png").is_file());
        assert!(dst_cache.join("paimon_tpl.npz").is_file());

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
}
