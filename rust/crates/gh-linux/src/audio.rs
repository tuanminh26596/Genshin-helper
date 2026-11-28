//! PCM audio feedback for Linux using `rodio`.
//!
//! Synthesizes the same 44.1 kHz tones as `gh-win32::audio`.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use tracing::warn;

pub const SAMPLE_RATE: u32 = 44_100;
const MAX_QUEUE_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneNote {
    pub freq_hz: f64,
    pub dur_ms: u32,
}

pub const TONES_SUCCESS: &[ToneNote] = &[
    ToneNote { freq_hz: 523.25, dur_ms: 65 },
    ToneNote { freq_hz: 659.25, dur_ms: 115 },
];

pub const TONES_INFO: &[ToneNote] = &[ToneNote { freq_hz: 587.33, dur_ms: 95 }];

pub const TONES_WARNING: &[ToneNote] = &[
    ToneNote { freq_hz: 392.0, dur_ms: 80 },
    ToneNote { freq_hz: 329.63, dur_ms: 105 },
];

pub const TONES_ERROR: &[ToneNote] = &[ToneNote { freq_hz: 246.94, dur_ms: 130 }];

pub fn tones_for_kind(kind: &str) -> &'static [ToneNote] {
    match kind {
        "success" => TONES_SUCCESS,
        "warning" => TONES_WARNING,
        "error" => TONES_ERROR,
        _ => TONES_INFO,
    }
}

/// Synthesizes a PCM WAV buffer for the given sequence of tones.
pub fn synthesize_wav(notes: &[ToneNote]) -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();

    for note in notes {
        let n_samples = (SAMPLE_RATE as f64 * note.dur_ms as f64 / 1000.0) as usize;
        let fade_samples = (n_samples / 10).min(100);

        for i in 0..n_samples {
            let t = i as f64 / SAMPLE_RATE as f64;
            let mut s = (2.0 * PI * note.freq_hz * t).sin();

            // Fade in/out
            if i < fade_samples {
                s *= i as f64 / fade_samples as f64;
            } else if i > n_samples - fade_samples {
                s *= (n_samples - i) as f64 / fade_samples as f64;
            }

            samples.push((s * 28000.0) as i16);
        }
    }

    // Build WAV header
    let data_len = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_len);

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((data_len + 36) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in &samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }

    wav
}

/// Audio feedback player using `rodio`.
pub struct AudioFeedback {
    shared: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    running: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    volume: Arc<AtomicI32>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for AudioFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioFeedback").finish()
    }
}

impl Default for AudioFeedback {
    fn default() -> Self {
        Self::new(true, 100)
    }
}

impl AudioFeedback {
    pub fn new(enabled: bool, volume: i32) -> Self {
        let shared = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let running = Arc::new(AtomicBool::new(true));
        let enabled_flag = Arc::new(AtomicBool::new(enabled));
        let volume_val = Arc::new(AtomicI32::new(volume.clamp(0, 100)));

        let shared_clone = Arc::clone(&shared);
        let running_clone = Arc::clone(&running);

        let thread = std::thread::spawn(move || {
            audio_player_thread(shared_clone, running_clone);
        });

        Self {
            shared,
            running,
            enabled: enabled_flag,
            volume: volume_val,
            thread: Mutex::new(Some(thread)),
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        self.enabled.store(enabled, Ordering::Release);
        enabled
    }

    pub fn set_volume(&self, volume: i32) -> i32 {
        let clamped = volume.clamp(0, 100);
        self.volume.store(clamped, Ordering::Release);
        clamped
    }

    pub fn set_volume_f64(&self, volume: f64) -> i32 {
        self.set_volume(volume.round() as i32)
    }

    pub fn play(&self, kind: &str, force: bool) -> bool {
        if !self.running.load(Ordering::Acquire) {
            return false;
        }
        let vol = self.volume.load(Ordering::Acquire);
        if !force && (!self.enabled.load(Ordering::Acquire) || vol <= 0) {
            return false;
        }
        let notes = tones_for_kind(kind);
        let wav = synthesize_wav(notes);
        let (lock, cond) = &*self.shared;
        let mut q = lock.lock().unwrap();
        if q.len() < MAX_QUEUE_LEN {
            q.push_back(wav);
            cond.notify_one();
            true
        } else {
            false
        }
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
        self.running.store(false, Ordering::SeqCst);
        self.shared.1.notify_all();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioFeedback {
    fn drop(&mut self) {
        self.close();
    }
}

fn audio_player_thread(
    shared: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    running: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::io::Cursor;

    // Try to init rodio output stream
    let stream_result = rodio::OutputStream::try_default();
    let (_stream, stream_handle) = match stream_result {
        Ok(r) => r,
        Err(e) => {
            warn!("gh-linux: audio output unavailable: {e}");
            return;
        }
    };

    let (lock, cond) = &*shared;
    while running.load(std::sync::atomic::Ordering::SeqCst) {
        let wav = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(w) = q.pop_front() {
                    break Some(w);
                }
                if !running.load(std::sync::atomic::Ordering::SeqCst) {
                    break None;
                }
                q = cond.wait(q).unwrap();
            }
        };

        if let Some(wav_data) = wav {
            let cursor = Cursor::new(wav_data);
            if let Ok(source) = rodio::Decoder::new(cursor) {
                let _ = stream_handle.play_raw(
                    rodio::source::SamplesConverter::new(source),
                );
                // Brief sleep to let it play (non-blocking rodio)
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
}
