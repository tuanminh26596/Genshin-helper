//! `--record-minimap` bounded async lossless developer benchmark recorder (`src/minimap_recorder.py`).

use crate::diagnostics::local_timestamp_compact_and_iso;
use gh_core::atomic_io::write_json_atomic;
use gh_core::frame::FrameBuf;
use gh_cv::image_io::write_image_frame;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

struct RecItem {
    idx: usize,
    img: FrameBuf,
    meta: Value,
}

struct RecorderShared {
    queue: VecDeque<RecItem>,
    max_queue: usize,
    stopped: bool,
    frames: usize,
    dropped: usize,
    write_errors: usize,
}

struct RecorderState {
    last_ts: Option<f64>,
    finished: bool,
    worker_started: bool,
    worker: Option<JoinHandle<()>>,
}

pub type RecorderDoneCallback = Box<dyn FnOnce(PathBuf, usize) + Send>;

pub struct MinimapRecorder {
    seconds: f64,
    fps: f64,
    target_frames: usize,
    out_dir: PathBuf,
    frames_dir: PathBuf,
    on_done: Mutex<Option<RecorderDoneCallback>>,
    shared: Arc<(Mutex<RecorderShared>, Condvar)>,
    state: Mutex<RecorderState>,
}

impl MinimapRecorder {
    pub fn new(
        seconds: f64,
        out_root: &Path,
        fps: f64,
        max_queue: usize,
        on_done: Option<RecorderDoneCallback>,
    ) -> Arc<Self> {
        let sec = if seconds.is_finite() {
            seconds.max(1.0)
        } else {
            1.0
        };
        let fps_val = if fps.is_finite() { fps.max(1.0) } else { 30.0 };
        let target_frames = (sec * fps_val).round().max(1.0) as usize;
        let (stamp, _) = local_timestamp_compact_and_iso();
        let mut out_dir = out_root.join(format!("rec_{stamp}"));
        if out_dir.exists() {
            for idx in 1..=99u32 {
                let cand = out_root.join(format!("rec_{stamp}_{idx:02}"));
                if !cand.exists() {
                    out_dir = cand;
                    break;
                }
            }
        }
        let frames_dir = out_dir.join("frames");

        Arc::new(Self {
            seconds: sec,
            fps: fps_val,
            target_frames,
            out_dir,
            frames_dir,
            on_done: Mutex::new(on_done),
            shared: Arc::new((
                Mutex::new(RecorderShared {
                    queue: VecDeque::new(),
                    max_queue: max_queue.max(8),
                    stopped: false,
                    frames: 0,
                    dropped: 0,
                    write_errors: 0,
                }),
                Condvar::new(),
            )),
            state: Mutex::new(RecorderState {
                last_ts: None,
                finished: false,
                worker_started: false,
                worker: None,
            }),
        })
    }

    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    pub fn active(&self) -> bool {
        !self.state.lock().unwrap_or_else(|e| e.into_inner()).finished
    }

    pub fn add(
        self: &Arc<Self>,
        hud_img: Option<&FrameBuf>,
        frame_w: i32,
        frame_h: i32,
        ts: f64,
        meta: Option<Map<String, Value>>,
    ) {
        let Some(img) = hud_img else {
            return;
        };
        if img.data.is_empty() || img.width == 0 || img.height == 0 {
            return;
        }

        let (idx, reached_target) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.finished {
                return;
            }
            if let Some(prev) = st.last_ts
                && (ts - prev) < (1.0 / self.fps) * 0.9
            {
                return;
            }
            if !st.worker_started
                && let Err(e) = self.start_worker_locked(&mut st)
            {
                tracing::warn!("[Recorder] Failed to start worker: {e}");
                st.finished = true;
                return;
            }
            st.last_ts = Some(ts);

            let (lock, _) = &*self.shared;
            let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            sh.frames += 1;
            let idx = sh.frames;
            (idx, idx >= self.target_frames)
        };

        let mut rec = Map::new();
        rec.insert("i".to_owned(), json!(idx));
        rec.insert(
            "ts".to_owned(),
            json!(((ts * 10_000.0).round()) / 10_000.0),
        );
        rec.insert("frame_w".to_owned(), json!(frame_w));
        rec.insert("frame_h".to_owned(), json!(frame_h));
        if let Some(extra) = meta {
            for (k, v) in extra {
                rec.insert(k, v);
            }
        }

        {
            let (lock, cv) = &*self.shared;
            let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            if sh.queue.len() >= sh.max_queue {
                sh.dropped += 1;
            } else {
                sh.queue.push_back(RecItem {
                    idx,
                    img: img.clone(),
                    meta: Value::Object(rec),
                });
                cv.notify_one();
            }
        }

        if reached_target {
            self.finish();
        }
    }

    fn start_worker_locked(self: &Arc<Self>, st: &mut RecorderState) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.frames_dir)?;
        let meta_file = File::create(self.out_dir.join("meta.jsonl"))?;
        let writer = BufWriter::new(meta_file);
        let rec = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("MinimapRecorder".to_owned())
            .spawn(move || rec.run_worker(writer))?;
        st.worker_started = true;
        st.worker = Some(handle);
        tracing::info!(
            "[Recorder] Recording {:.0}s of minimap frames -> {}",
            self.seconds,
            self.out_dir.display()
        );
        Ok(())
    }

    fn run_worker(&self, mut meta_writer: BufWriter<File>) {
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

            let path = self.frames_dir.join(format!("{:06}.png", item.idx));
            let write_ok = write_image_frame(&path, item.img.as_view(), Some("png")).is_ok();
            let line_ok = serde_json::to_string(&item.meta)
                .map_err(std::io::Error::other)
                .and_then(|s| writeln!(meta_writer, "{s}"));
            if !write_ok || line_ok.is_err() {
                let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                sh.write_errors += 1;
            }
        }

        let _ = meta_writer.flush();
        let (frames, dropped, write_errors) = {
            let sh = lock.lock().unwrap_or_else(|e| e.into_inner());
            (sh.frames, sh.dropped, sh.write_errors)
        };
        let info = json!({
            "frames": frames,
            "dropped": dropped,
            "write_errors": write_errors,
            "fps": self.fps,
            "seconds": self.seconds,
        });
        let _ = write_json_atomic(&self.out_dir.join("info.json"), &info);
        tracing::info!(
            "[Recorder] Done: {} frames ({} dropped) -> {}",
            frames,
            dropped,
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
        // Wait up to `timeout` using bounded polling to avoid hanging indefinitely on shutdown.
        let deadline = std::time::Instant::now() + timeout;
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::frame::PixelFormat;
    use image::ImageReader;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static REC_TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_rec_root(tag: &str) -> PathBuf {
        let seq = REC_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_rec_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn lazy_init_avoids_dir_when_unused_and_preserves_bgra_alpha_on_record() {
        let root = temp_rec_root("lazy");
        let rec_unused = MinimapRecorder::new(2.0, &root, 10.0, 16, None);
        let unused_dir = rec_unused.out_dir().to_path_buf();
        rec_unused.finish();
        rec_unused.join_timeout(Duration::from_secs(15));
        assert!(!unused_dir.exists(), "unused recorder must not create directory");

        let done_frames = Arc::new(AtomicUsize::new(0));
        let done_clone = Arc::clone(&done_frames);
        let rec = MinimapRecorder::new(
            1.0,
            &root,
            10.0,
            16,
            Some(Box::new(move |_dir, n| {
                done_clone.store(n, Ordering::Release);
            })),
        );

        let mut bgra = FrameBuf::new_zeroed(24, 24, PixelFormat::Bgra8);
        for px in bgra.data.as_chunks_mut::<4>().0 {
            px[0] = 10;
            px[1] = 20;
            px[2] = 30;
            px[3] = 77;
        }

        // Frame 1 accepted
        rec.add(Some(&bgra), 1920, 1080, 0.0, None);
        // Sub-interval frame throttled (dt = 0.02 < 0.09)
        rec.add(Some(&bgra), 1920, 1080, 0.02, None);

        // Frames 2..=10 accepted
        for i in 1..10usize {
            let mut m = Map::new();
            m.insert("state".to_owned(), json!("TRACKING"));
            rec.add(Some(&bgra), 1920, 1080, (i as f64) * 0.1, Some(m));
        }

        assert!(!rec.active(), "recorder should auto-finish after target_frames");
        rec.join_timeout(Duration::from_secs(15));
        assert_eq!(done_frames.load(Ordering::Acquire), 10);

        let out_dir = rec.out_dir();
        let meta_lines: Vec<String> = std::fs::read_to_string(out_dir.join("meta.jsonl"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(meta_lines.len(), 10);
        for line in &meta_lines {
            let _: Value = serde_json::from_str(line).unwrap();
        }

        let info: Value =
            serde_json::from_str(&std::fs::read_to_string(out_dir.join("info.json")).unwrap())
                .unwrap();
        assert_eq!(info["frames"], json!(10));
        assert_eq!(info["dropped"], json!(0));
        assert_eq!(info["write_errors"], json!(0));

        // Verify Bgra8 alpha channel (77) survived PNG roundtrip
        let png = ImageReader::open(out_dir.join("frames").join("000001.png"))
            .unwrap()
            .decode()
            .unwrap()
            .to_rgba8();
        assert_eq!(png.get_pixel(5, 5).0[3], 77);

        let _ = std::fs::remove_dir_all(&root);
    }
}
