//! Thread-safe in-memory ring buffer capturing recent `tracing` events for diagnostic bundles
//! (`06_app_log.txt`) without writing continuous runtime logs to disk.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
#[cfg(windows)]
use windows::Win32::System::SystemInformation::GetLocalTime;

pub const DEFAULT_LOG_CAPACITY: usize = 300;
pub const DEFAULT_MAX_LINE_CHARS: usize = 512;

#[derive(Clone)]
pub struct LogRingBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
    max_line_chars: usize,
}

impl Default for LogRingBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_LOG_CAPACITY, DEFAULT_MAX_LINE_CHARS)
    }
}

impl LogRingBuffer {
    pub fn new(capacity: usize, max_line_chars: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            capacity: cap,
            max_line_chars: max_line_chars.max(32),
        }
    }

    pub fn push_line(&self, line: &str) {
        let single_line = line.replace(['\r', '\n'], " ");
        let trimmed = single_line.trim();
        if trimmed.is_empty() {
            return;
        }
        let clamped = truncate_chars(trimmed, self.max_line_chars);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while guard.len() >= self.capacity {
            guard.pop_front();
        }
        guard.push_back(clamped);
    }

    pub fn snapshot_lines(&self) -> Vec<String> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.iter().cloned().collect()
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_owned();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

#[derive(Default)]
struct EventFieldVisitor {
    message: Option<String>,
    extras: Vec<(String, String)>,
}

impl Visit for EventFieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.extras.push((field.name().to_owned(), rendered));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.extras.push((field.name().to_owned(), value.to_owned()));
        }
    }
}

fn format_level(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

impl<S: Subscriber> Layer<S> for LogRingBuffer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = EventFieldVisitor::default();
        event.record(&mut visitor);

        #[cfg(windows)]
        let (hour, minute, second, ms) = {
            // SAFETY: `GetLocalTime` fills stack-allocated `SYSTEMTIME`.
            let st = unsafe { GetLocalTime() };
            (st.wHour, st.wMinute, st.wSecond, st.wMilliseconds)
        };
        #[cfg(not(windows))]
        let (_, _, _, hour, minute, second, ms) = gh_platform::sys_info::local_time_parts();

        let ts = format!("{:02}:{:02}:{:02}.{:03}", hour, minute, second, ms);

        let mut msg = visitor.message.unwrap_or_default();
        if !visitor.extras.is_empty() {
            let extra_str: Vec<String> = visitor
                .extras
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            if msg.is_empty() {
                msg = extra_str.join(" ");
            } else {
                msg = format!("{msg} {}", extra_str.join(" "));
            }
        }

        let line = format!(
            "[{ts}] {:<5} {}: {}",
            format_level(meta.level()),
            meta.target(),
            msg
        );
        self.push_line(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_bounds_capacity_and_line_length() {
        let buf = LogRingBuffer::new(3, 40);
        buf.push_line("line 1");
        buf.push_line("line 2");
        buf.push_line("line 3");
        buf.push_line("line 4 with a very long payload exceeding forty characters total");

        let lines = buf.snapshot_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line 2");
        assert_eq!(lines[1], "line 3");
        assert!(lines[2].ends_with("..."));
        assert!(lines[2].chars().count() <= 40);
    }
}
