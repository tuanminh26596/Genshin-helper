//! Atomic file write helpers (temp + fsync + replace with Windows lock retries 5/32/33).
//! Ported from `src/image_io.py`.

use crate::{CoreError, CoreResult};
use serde::Serialize;
use serde_json::ser::{PrettyFormatter, Serializer};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const DEFAULT_REPLACE_RETRIES: usize = 5;
pub const DEFAULT_REPLACE_RETRY_DELAY: Duration = Duration::from_millis(50);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns `true` for transient file-lock / sharing errors worth retrying on replace.
pub fn is_retryable_replace_error(err: &std::io::Error) -> bool {
    if err.kind() == ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33)
        if matches!(err.raw_os_error(), Some(5 | 32 | 33)) {
            return true;
        }
    }
    #[cfg(unix)]
    {
        // EPERM (1), EACCES (13), EBUSY (16)
        if matches!(err.raw_os_error(), Some(1 | 13 | 16)) {
            return true;
        }
    }
    false
}

/// Replaces `dst` with `src` using `std::fs::rename`, retrying on transient lock errors.
pub fn replace_with_retry(
    src: &Path,
    dst: &Path,
    retries: usize,
    retry_delay: Duration,
) -> CoreResult<()> {
    replace_with_retry_using(src, dst, retries, retry_delay, |s, d| std::fs::rename(s, d))
}

/// Testable retry wrapper around an arbitrary `replace_fn`.
pub fn replace_with_retry_using<F>(
    src: &Path,
    dst: &Path,
    retries: usize,
    retry_delay: Duration,
    mut replace_fn: F,
) -> CoreResult<()>
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let max_attempts = retries.saturating_add(1);
    for attempt in 0..max_attempts {
        match replace_fn(src, dst) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt + 1 < max_attempts && is_retryable_replace_error(&err) {
                    if !retry_delay.is_zero() {
                        std::thread::sleep(retry_delay);
                    }
                    continue;
                }
                return Err(CoreError::Io(err));
            }
        }
    }
    unreachable!("loop always returns when max_attempts >= 1")
}

/// RAII sibling temporary file that automatically cleans up on drop unless committed.
pub struct AtomicTempFile {
    file: Option<File>,
    temp_path: PathBuf,
    dst: PathBuf,
    committed: bool,
}

impl AtomicTempFile {
    pub fn new(dst: &Path, prefix: &str) -> CoreResult<Self> {
        let parent = match dst.parent() {
            Some(p) if !p.as_os_str().is_empty() => {
                std::fs::create_dir_all(p)?;
                p
            }
            _ => Path::new("."),
        };

        let sanitized_prefix = if prefix.trim().is_empty() {
            "tmp"
        } else {
            prefix.trim().trim_start_matches('.')
        };
        let pid = std::process::id();

        for _ in 0..1024 {
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temp_name = format!(".{sanitized_prefix}-{pid}-{seq}.tmp");
            let temp_path = parent.join(temp_name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        temp_path,
                        dst: dst.to_path_buf(),
                        committed: false,
                    });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(CoreError::Io(err)),
            }
        }

        Err(CoreError::Io(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "failed to allocate unique temporary file",
        )))
    }

    pub fn path(&self) -> &Path {
        &self.temp_path
    }

    pub fn as_file_mut(&mut self) -> CoreResult<&mut File> {
        self.file.as_mut().ok_or_else(|| {
            CoreError::InvalidData("temporary file handle already closed".to_owned())
        })
    }

    pub fn commit(mut self, retries: usize, retry_delay: Duration) -> CoreResult<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
            drop(file);
        }
        replace_with_retry(&self.temp_path, &self.dst, retries, retry_delay)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for AtomicTempFile {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.temp_path);
        }
    }
}

/// Atomically writes `data` to `path` using default retry settings (`5 x 50ms`).
pub fn write_bytes_atomic(path: &Path, data: &[u8]) -> CoreResult<()> {
    write_bytes_atomic_with_retry(
        path,
        data,
        DEFAULT_REPLACE_RETRIES,
        DEFAULT_REPLACE_RETRY_DELAY,
    )
}

/// Atomically writes `data` to `path` with configurable replace retry policy.
pub fn write_bytes_atomic_with_retry(
    path: &Path,
    data: &[u8],
    retries: usize,
    retry_delay: Duration,
) -> CoreResult<()> {
    let mut tmp = AtomicTempFile::new(path, "bytes")?;
    tmp.as_file_mut()?.write_all(data)?;
    tmp.commit(retries, retry_delay)
}

/// Atomically writes UTF-8 text to `path`.
pub fn write_str_atomic(path: &Path, text: &str) -> CoreResult<()> {
    let mut tmp = AtomicTempFile::new(path, "text")?;
    tmp.as_file_mut()?.write_all(text.as_bytes())?;
    tmp.commit(DEFAULT_REPLACE_RETRIES, DEFAULT_REPLACE_RETRY_DELAY)
}

/// Serializes `value` as 4-space indented UTF-8 JSON without ASCII escaping (`ensure_ascii=False`).
pub fn to_json_pretty_4<T: Serialize + ?Sized>(value: &T) -> CoreResult<String> {
    let mut buf = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"    ");
    let mut ser = Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut ser)?;
    String::from_utf8(buf).map_err(|e| CoreError::InvalidData(format!("non-UTF-8 JSON: {e}")))
}

/// Atomically writes `value` as 4-space indented UTF-8 JSON with default retry settings.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> CoreResult<()> {
    write_json_atomic_with_retry(
        path,
        value,
        DEFAULT_REPLACE_RETRIES,
        DEFAULT_REPLACE_RETRY_DELAY,
    )
}

/// Atomically writes `value` as 4-space indented UTF-8 JSON with custom retry settings.
pub fn write_json_atomic_with_retry<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
    retries: usize,
    retry_delay: Duration,
) -> CoreResult<()> {
    let json = to_json_pretty_4(value)?;
    let mut tmp = AtomicTempFile::new(path, "json")?;
    tmp.as_file_mut()?.write_all(json.as_bytes())?;
    tmp.commit(retries, retry_delay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_core_atomic_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn write_json_atomic_uses_4_space_utf8_and_cleans_temps() {
        let base = make_temp_dir("utf8");
        let target = base.join("tiếng_việt").join("thiết_lập.json");
        let payload = json!({
            "language": "vi",
            "instructions": "Ngôn ngữ tiếng Việt — phím tắt",
            "nested": {
                "enabled": true
            }
        });

        write_json_atomic(&target, &payload).expect("write_json_atomic");

        let text = std::fs::read_to_string(&target).expect("read back utf8 json");
        assert!(text.contains("\n    \"instructions\": \"Ngôn ngữ tiếng Việt — phím tắt\""));
        assert!(text.contains("\n        \"enabled\": true"));
        assert!(!text.contains("\\u"));

        let parent = target.parent().expect("parent");
        let entries: Vec<_> = std::fs::read_dir(parent)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "thiết_lập.json");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn replace_with_retry_retries_sharing_and_permission_errors() {
        #[cfg(windows)]
        let raw_codes: &[i32] = &[5, 32, 33];
        #[cfg(not(windows))]
        let raw_codes: &[i32] = &[1, 13]; // EPERM, EACCES
        for &raw_code in raw_codes {
            let mut calls = 0usize;
            replace_with_retry_using(
                Path::new("a"),
                Path::new("b"),
                5,
                Duration::ZERO,
                |_, _| {
                    calls += 1;
                    if calls < 3 {
                        Err(std::io::Error::from_raw_os_error(raw_code))
                    } else {
                        Ok(())
                    }
                },
            )
            .expect("succeeds on 3rd attempt");
            assert_eq!(calls, 3);
        }

        let mut perm_calls = 0usize;
        replace_with_retry_using(
            Path::new("a"),
            Path::new("b"),
            5,
            Duration::ZERO,
            |_, _| {
                perm_calls += 1;
                if perm_calls < 3 {
                    Err(std::io::Error::new(ErrorKind::PermissionDenied, "locked"))
                } else {
                    Ok(())
                }
            },
        )
        .expect("succeeds on PermissionDenied retry");
        assert_eq!(perm_calls, 3);

        // Exhaustion fails after retries + 1 attempts
        let mut exhaust_calls = 0usize;
        let err = replace_with_retry_using(
            Path::new("a"),
            Path::new("b"),
            5,
            Duration::ZERO,
            |_, _| {
                exhaust_calls += 1;
                Err(std::io::Error::new(ErrorKind::PermissionDenied, "still locked"))
            },
        )
        .expect_err("exhausted retries must fail");
        assert_eq!(exhaust_calls, 6);
        assert!(matches!(err, CoreError::Io(_)));

        // Non-retryable error fails on attempt 1
        let mut nf_calls = 0usize;
        let err = replace_with_retry_using(
            Path::new("a"),
            Path::new("b"),
            5,
            Duration::ZERO,
            |_, _| {
                nf_calls += 1;
                Err(std::io::Error::new(ErrorKind::NotFound, "missing"))
            },
        )
        .expect_err("NotFound must not retry");
        assert_eq!(nf_calls, 1);
        assert!(matches!(err, CoreError::Io(_)));
    }

    #[test]
    fn failed_atomic_temp_file_cleans_up_on_drop() {
        let base = make_temp_dir("drop_cleanup");
        let dst = base.join("out.bin");
        let temp_path = {
            let mut tmp = AtomicTempFile::new(&dst, "cleanup").expect("create temp");
            tmp.as_file_mut()
                .expect("file")
                .write_all(b"partial")
                .expect("write");
            let p = tmp.path().to_path_buf();
            assert!(p.is_file());
            p
            // tmp dropped without commit
        };
        assert!(!temp_path.exists());
        assert!(!dst.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
