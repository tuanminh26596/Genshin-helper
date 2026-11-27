//! Synthesized 44.1 kHz PCM WAV headphone-safe audio chimes via `PlaySoundW(SND_MEMORY)` (`src/single_monitor.py`).

use std::collections::{HashMap, VecDeque};
use std::f64::consts::PI;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

pub const SAMPLE_RATE: u32 = 44_100;
const MAX_QUEUE_LEN: usize = 4;
const MAX_WAV_CACHE_ENTRIES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneNote {
    pub freq_hz: f64,
    pub dur_ms: u32,
}

pub const TONES_SUCCESS: &[ToneNote] = &[
    ToneNote {
        freq_hz: 523.25,
        dur_ms: 65,
    },
    ToneNote {
        freq_hz: 659.25,
        dur_ms: 115,
    },
];

pub const TONES_INFO: &[ToneNote] = &[ToneNote {
    freq_hz: 587.33,
    dur_ms: 95,
}];

pub const TONES_WARNING: &[ToneNote] = &[
    ToneNote {
        freq_hz: 392.0,
        dur_ms: 80,
    },
    ToneNote {
        freq_hz: 329.63,
        dur_ms: 105,
    },
];

pub const TONES_ERROR: &[ToneNote] = &[ToneNote {
    freq_hz: 246.94,
    dur_ms: 130,
}];

pub fn tones_for_kind(kind: &str) -> &'static [ToneNote] {
    match kind {
        "success" => TONES_SUCCESS,
        "warning" => TONES_WARNING,
        "error" => TONES_ERROR,
        _ => TONES_INFO,
    }
}

fn normalize_kind(kind: &str) -> &'static str {
    match kind {
        "success" => "success",
        "warning" => "warning",
        "error" => "error",
        _ => "info",
    }
}

pub fn clamp_volume(volume: i32) -> i32 {
    volume.clamp(0, 100)
}

pub fn clamp_volume_f64(volume: f64) -> i32 {
    if !volume.is_finite() {
        return 50;
    }
    volume.clamp(0.0, 100.0).round_ties_even() as i32
}

fn smooth_edge_env(idx: usize, n: usize, sr: f64, attack_s: f64, tail_s: f64) -> f64 {
    let att_n = ((attack_s * sr).round_ties_even() as usize).clamp(1, n);
    let tail_n = ((tail_s * sr).round_ties_even() as usize).clamp(2, n);
    let att = if idx < att_n {
        0.5 * (1.0 - (PI * (idx as f64) / (att_n as f64)).cos())
    } else {
        1.0
    };
    let tail_start = n - tail_n;
    let tail = if idx >= tail_start && tail_n > 1 {
        1.0 - ((idx - tail_start) as f64 / ((tail_n - 1) as f64))
    } else {
        1.0
    };
    att * tail
}

fn mix_silk_pluck(
    buf: &mut [f64],
    sr: f64,
    start_s: f64,
    freq_hz: f64,
    dur_s: f64,
    amp: f64,
    brightness: f64,
) {
    let start_idx = (start_s * sr).round_ties_even().max(0.0) as usize;
    let note_samples = (dur_s * sr).round_ties_even().max(1.0) as usize;
    let tau = dur_s * 0.36;
    let w = 2.0 * PI * freq_hz;
    let transient_w = 2.0 * PI * (freq_hz * 3.1).min(2800.0);
    let limit = buf.len().saturating_sub(start_idx).min(note_samples);
    for i in 0..limit {
        let t = i as f64 / sr;
        let env = (-t / tau).exp() * smooth_edge_env(i, note_samples, sr, 0.0022, 0.012);
        let body = (w * t).sin()
            + (0.40 * brightness) * (2.0 * w * t).sin() * (-t / 0.055).exp()
            + (0.18 * brightness) * (3.0 * w * t).sin() * (-t / 0.032).exp()
            + (0.07 * brightness) * (4.0 * w * t).sin() * (-t / 0.018).exp()
            + 0.03 * (transient_w * t).sin() * (-t / 0.002).exp();
        buf[start_idx + i] += amp * env * body;
    }
}

/// Synthesizes a soft 16-bit mono 44.1 kHz PCM RIFF/WAVE Liyue silk-guzheng chime.
pub fn synthesize_chime_wav(kind: &str, volume: i32) -> Vec<u8> {
    let vol = clamp_volume(volume);
    if vol <= 0 {
        return Vec::new();
    }

    type PluckNote = (f64, f64, f64, f64, f64);

    let sr = f64::from(SAMPLE_RATE);
    let (total_s, target_peak, notes): (f64, f64, &[PluckNote]) =
        match normalize_kind(kind) {
            "success" => (
                0.40,
                16_500.0,
                &[
                    (392.00, 0.000, 0.26, 0.72, 1.0),
                    (493.88, 0.044, 0.26, 0.76, 1.0),
                    (587.33, 0.088, 0.26, 0.84, 1.0),
                    (783.99, 0.132, 0.26, 0.94, 1.0),
                ],
            ),
            "warning" => (
                0.23,
                15_400.0,
                &[
                    (659.25, 0.000, 0.15, 0.84, 0.45),
                    (587.33, 0.058, 0.16, 0.88, 0.40),
                ],
            ),
            "error" => (
                0.24,
                15_500.0,
                &[
                    (196.00, 0.000, 0.14, 0.90, 0.35),
                    (146.83, 0.066, 0.16, 0.95, 0.30),
                ],
            ),
            _ => (
                0.25,
                16_000.0,
                &[
                    (587.33, 0.000, 0.17, 0.80, 0.95),
                    (880.00, 0.048, 0.19, 0.92, 1.0),
                ],
            ),
        };

    let total_samples = (total_s * sr).round_ties_even().max(1.0) as usize;
    let mut buf = vec![0.0_f64; total_samples];
    for &(freq_hz, start_s, dur_s, amp, brightness) in notes {
        mix_silk_pluck(&mut buf, sr, start_s, freq_hz, dur_s, amp, brightness);
    }

    let raw_peak = buf.iter().fold(0.0_f64, |acc, &x| acc.max(x.abs()));
    let scale = if raw_peak > 1e-9 {
        (target_peak / 32_767.0) / raw_peak * (f64::from(vol) / 100.0)
    } else {
        0.0
    };

    let data_bytes_len = (total_samples * 2) as u32;
    let riff_size = 36u32 + data_bytes_len;

    let mut wav = Vec::with_capacity(44 + data_bytes_len as usize);
    // Standard 44-byte RIFF/WAVE PCM header (mono, 16-bit, 44100 Hz)
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // Mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // ByteRate
    wav.extend_from_slice(&2u16.to_le_bytes()); // BlockAlign
    wav.extend_from_slice(&16u16.to_le_bytes()); // BitsPerSample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes_len.to_le_bytes());

    for &s in &buf {
        let sample = (s * scale * 32_767.0)
            .round_ties_even()
            .clamp(-32_767.0, 32_767.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    wav
}

pub type AudioSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

fn winmm_play_wav_sync(wav_bytes: &[u8]) {
    if wav_bytes.is_empty() {
        return;
    }
    // SAFETY: `SND_MEMORY` treats `pszsound` as a pointer to an in-memory RIFF/WAVE image.
    // Playback is synchronous (`SND_SYNC == 0`), so `wav_bytes` stays alive for the whole call.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(wav_bytes.as_ptr().cast::<u16>()),
            None,
            SND_MEMORY | SND_NODEFAULT,
        );
    }
}

struct AudioSharedState {
    enabled: bool,
    volume: i32,
    closed: bool,
    queue: VecDeque<(&'static str, i32)>,
    wav_cache: HashMap<(&'static str, i32), Arc<[u8]>>,
    worker_started: bool,
}

struct AudioWorkerInner {
    state: Mutex<AudioSharedState>,
    cv: Condvar,
    thread: Mutex<Option<JoinHandle<()>>>,
    sink: AudioSink,
}

/// Non-blocking headphone-safe audio chime worker using in-memory synthesized PCM WAV.
///
/// The `"GH_AudioFeedback"` worker thread starts lazily on the first accepted `play()` call.
pub struct AudioFeedback {
    inner: Arc<AudioWorkerInner>,
}

impl Default for AudioFeedback {
    fn default() -> Self {
        Self::new(true, 50)
    }
}

impl AudioFeedback {
    pub fn new(enabled: bool, volume: i32) -> Self {
        Self::with_sink(enabled, volume, Arc::new(winmm_play_wav_sync))
    }

    pub fn with_sink(enabled: bool, volume: i32, sink: AudioSink) -> Self {
        Self {
            inner: Arc::new(AudioWorkerInner {
                state: Mutex::new(AudioSharedState {
                    enabled,
                    volume: clamp_volume(volume),
                    closed: false,
                    queue: VecDeque::with_capacity(MAX_QUEUE_LEN),
                    wav_cache: HashMap::new(),
                    worker_started: false,
                }),
                cv: Condvar::new(),
                thread: Mutex::new(None),
                sink,
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("audio state lock poisoned")
            .enabled
    }

    pub fn volume(&self) -> i32 {
        self.inner
            .state
            .lock()
            .expect("audio state lock poisoned")
            .volume
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        let mut guard = self.inner.state.lock().expect("audio state lock poisoned");
        guard.enabled = enabled;
        guard.enabled
    }

    pub fn set_volume(&self, volume: i32) -> i32 {
        let mut guard = self.inner.state.lock().expect("audio state lock poisoned");
        guard.volume = clamp_volume(volume);
        guard.volume
    }

    pub fn set_volume_f64(&self, volume: f64) -> i32 {
        let mut guard = self.inner.state.lock().expect("audio state lock poisoned");
        guard.volume = clamp_volume_f64(volume);
        guard.volume
    }

    pub fn has_worker(&self) -> bool {
        let guard = self.inner.thread.lock().expect("audio thread lock poisoned");
        guard.as_ref().is_some_and(|h| !h.is_finished())
    }

    fn get_or_synthesize_wav(inner: &AudioWorkerInner, kind: &'static str, vol: i32) -> Arc<[u8]> {
        let key = (kind, vol);
        {
            let guard = inner.state.lock().expect("audio state lock poisoned");
            if let Some(cached) = guard.wav_cache.get(&key) {
                return Arc::clone(cached);
            }
        }
        let wav: Arc<[u8]> = Arc::from(synthesize_chime_wav(kind, vol));
        let mut guard = inner.state.lock().expect("audio state lock poisoned");
        if guard.wav_cache.len() >= MAX_WAV_CACHE_ENTRIES {
            guard.wav_cache.clear();
        }
        guard.wav_cache.insert(key, Arc::clone(&wav));
        wav
    }

    fn ensure_worker(&self) {
        let mut thread_guard = self.inner.thread.lock().expect("audio thread lock poisoned");
        let needs_spawn = match thread_guard.as_ref() {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if !needs_spawn {
            return;
        }
        {
            let mut state_guard = self.inner.state.lock().expect("audio state lock poisoned");
            if state_guard.closed {
                return;
            }
            state_guard.worker_started = true;
        }
        let worker_inner = Arc::clone(&self.inner);
        if let Ok(handle) = std::thread::Builder::new()
            .name("GH_AudioFeedback".to_owned())
            .spawn(move || Self::worker_loop(worker_inner))
        {
            *thread_guard = Some(handle);
        }
    }

    fn worker_loop(inner: Arc<AudioWorkerInner>) {
        loop {
            let item = {
                let mut guard = inner.state.lock().expect("audio state lock poisoned");
                while guard.queue.is_empty() && !guard.closed {
                    guard = inner.cv.wait(guard).expect("audio state lock poisoned");
                }
                if guard.closed {
                    break;
                }
                guard.queue.pop_front()
            };

            let Some((kind, vol)) = item else {
                continue;
            };
            let wav_bytes = Self::get_or_synthesize_wav(&inner, kind, vol);
            if !wav_bytes.is_empty() {
                (inner.sink)(&wav_bytes);
            }
        }
    }

    pub fn play(&self, kind: &str, force: bool) -> bool {
        let eff_vol = {
            let guard = self.inner.state.lock().expect("audio state lock poisoned");
            if guard.closed {
                return false;
            }
            if !force && (!guard.enabled || guard.volume <= 0) {
                return false;
            }
            if force {
                guard.volume.max(1)
            } else {
                guard.volume
            }
        };

        self.ensure_worker();

        let norm_kind = normalize_kind(kind);
        {
            let mut guard = self.inner.state.lock().expect("audio state lock poisoned");
            if guard.closed {
                return false;
            }
            if guard.queue.len() >= MAX_QUEUE_LEN {
                guard.queue.pop_front();
            }
            guard.queue.push_back((norm_kind, eff_vol));
        }
        self.inner.cv.notify_one();
        true
    }

    pub fn play_success(&self) -> bool {
        self.play("success", false)
    }

    pub fn play_info(&self) -> bool {
        self.play("info", false)
    }

    pub fn play_warning(&self) -> bool {
        self.play("warning", false)
    }

    pub fn play_error(&self) -> bool {
        self.play("error", false)
    }

    pub fn close(&self) {
        {
            let mut guard = self.inner.state.lock().expect("audio state lock poisoned");
            if guard.closed {
                return;
            }
            guard.closed = true;
            guard.queue.clear();
        }
        self.inner.cv.notify_all();

        let handle = {
            let mut thread_guard = self.inner.thread.lock().expect("audio thread lock poisoned");
            thread_guard.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl Drop for AudioFeedback {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn pcm_peak_i16(wav: &[u8]) -> i32 {
        wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i32::from(i16::from_le_bytes(c)).abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn synthesize_chime_wav_header_and_amplitude_parity() {
        assert!(synthesize_chime_wav("success", 0).is_empty());
        assert!(synthesize_chime_wav("info", -5).is_empty());

        let wav50 = synthesize_chime_wav("success", 50);
        let wav100 = synthesize_chime_wav("success", 100);

        assert!(wav50.len() > 44);
        assert_eq!(&wav50[0..4], b"RIFF");
        assert_eq!(&wav50[8..12], b"WAVE");
        assert_eq!(&wav50[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav50[20], wav50[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav50[22], wav50[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav50[24], wav50[25], wav50[26], wav50[27]]),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes([wav50[34], wav50[35]]), 16);
        assert_eq!(&wav50[36..40], b"data");

        let data_len = u32::from_le_bytes([wav50[40], wav50[41], wav50[42], wav50[43]]) as usize;
        assert_eq!(wav50.len(), 44 + data_len);

        let peak50 = pcm_peak_i16(&wav50);
        let peak100 = pcm_peak_i16(&wav100);
        assert!((10_000..=22_000).contains(&peak100), "peak100={peak100}");
        let ratio = peak50 as f64 / peak100 as f64;
        assert!((0.45..=0.55).contains(&ratio), "ratio={ratio}");

        // Unknown kind falls back to "info"
        assert_eq!(
            synthesize_chime_wav("unknown_kind", 50),
            synthesize_chime_wav("info", 50)
        );
    }

    #[test]
    fn liyue_guzheng_cues_produce_distinct_headphone_safe_peaks() {
        let cues: Vec<(&str, i32)> = ["success", "info", "warning", "error"]
            .into_iter()
            .map(|kind| {
                let wav = synthesize_chime_wav(kind, 100);
                assert!(wav.len() > 8000, "{kind} too short");
                let peak = pcm_peak_i16(&wav);
                assert!(
                    (14_000..=18_000).contains(&peak),
                    "{kind} peak100={peak} outside headphone-safe bounds"
                );
                (kind, wav.len() as i32)
            })
            .collect();
        assert_ne!(cues[0].1, cues[1].1);
        assert_ne!(cues[1].1, cues[2].1);
    }

    #[test]
    fn volume_clamping_handles_bounds_and_non_finite() {
        assert_eq!(clamp_volume(-10), 0);
        assert_eq!(clamp_volume(0), 0);
        assert_eq!(clamp_volume(150), 100);

        assert_eq!(clamp_volume_f64(-10.0), 0);
        assert_eq!(clamp_volume_f64(150.0), 100);
        assert_eq!(clamp_volume_f64(75.6), 76);
        assert_eq!(clamp_volume_f64(f64::NAN), 50);
        assert_eq!(clamp_volume_f64(f64::INFINITY), 50);
        assert_eq!(clamp_volume_f64(f64::NEG_INFINITY), 50);
    }

    #[test]
    fn lazy_worker_play_and_close_with_mock_sink() {
        let count = Arc::new(AtomicUsize::new(0));
        let total_bytes = Arc::new(AtomicUsize::new(0));
        let count_cb = Arc::clone(&count);
        let bytes_cb = Arc::clone(&total_bytes);

        let audio = AudioFeedback::with_sink(
            true,
            50,
            Arc::new(move |wav| {
                count_cb.fetch_add(1, Ordering::SeqCst);
                bytes_cb.fetch_add(wav.len(), Ordering::SeqCst);
            }),
        );

        assert_eq!(audio.volume(), 50);
        assert!(audio.enabled());
        assert!(!audio.has_worker());

        assert!(audio.play("success", false));
        assert!(audio.has_worker());
        assert!(audio.play("warning", false));
        assert!(audio.play("error", false));

        audio.set_enabled(false);
        assert!(!audio.play("info", false));
        assert!(audio.play("info", true));

        let deadline = Instant::now() + Duration::from_secs(15);
        while count.load(Ordering::SeqCst) < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(count.load(Ordering::SeqCst), 4);
        assert!(total_bytes.load(Ordering::SeqCst) > 1000);

        audio.close();
        assert!(!audio.has_worker());
        assert!(!audio.play("success", true));
    }

    #[test]
    fn bounded_queue_drops_oldest_and_cache_caps_at_32() {
        let played_lens = Arc::new(Mutex::new(Vec::<usize>::new()));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new((Mutex::new(false), Condvar::new()));

        let played_cb = Arc::clone(&played_lens);
        let gate_cb = Arc::clone(&gate);
        let entered_cb = Arc::clone(&entered);
        let audio = AudioFeedback::with_sink(
            true,
            50,
            Arc::new(move |wav| {
                {
                    let (elock, ecv) = &*entered_cb;
                    let mut e = elock.lock().unwrap();
                    if !*e {
                        *e = true;
                        ecv.notify_all();
                    }
                }
                let (lock, cv) = &*gate_cb;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
                played_cb.lock().unwrap().push(wav.len());
            }),
        );

        // Start worker with first item and wait deterministically until it is blocked inside sink
        assert!(audio.play("info", false));
        {
            let (elock, ecv) = &*entered;
            let e = elock.lock().unwrap();
            let (e_guard, timeout_res) = ecv
                .wait_timeout_while(e, Duration::from_secs(15), |flag| !*flag)
                .unwrap();
            assert!(
                !timeout_res.timed_out() && *e_guard,
                "audio worker did not enter mock sink within timeout"
            );
        }

        // Enqueue 5 items while worker is blocked; queue depth 4 drops oldest ("info")
        assert!(audio.play("info", false)); // will be dropped
        assert!(audio.play("success", false));
        assert!(audio.play("warning", false));
        assert!(audio.play("error", false));
        assert!(audio.play("success", false));

        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        while played_lens.lock().unwrap().len() < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        audio.close();

        let lens = played_lens.lock().unwrap().clone();
        assert_eq!(lens.len(), 5);
        assert_eq!(lens[0], synthesize_chime_wav("info", 50).len());
        assert_eq!(lens[1], synthesize_chime_wav("success", 50).len());
        assert_eq!(lens[2], synthesize_chime_wav("warning", 50).len());
        assert_eq!(lens[3], synthesize_chime_wav("error", 50).len());
        assert_eq!(lens[4], synthesize_chime_wav("success", 50).len());

        // Verify cache clearing at 32 entries
        let audio2 = AudioFeedback::with_sink(true, 10, Arc::new(|_| {}));
        for v in 1..=35 {
            let _ = AudioFeedback::get_or_synthesize_wav(&audio2.inner, "info", v);
        }
        let cache_len = audio2.inner.state.lock().unwrap().wav_cache.len();
        assert!(cache_len <= MAX_WAV_CACHE_ENTRIES);
        assert!(cache_len < 35);
    }
}
