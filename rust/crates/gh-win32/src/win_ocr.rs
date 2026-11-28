//! In-process WinRT `OcrEngine` implementing `gh_core::traits::OcrProvider` (`src/win_ocr.py`).
//!
//! Strictly in-process (zero PowerShell spawning, zero temp `.ps1`/`.png` files).

use gh_core::frame::{FrameView, PixelFormat};
use gh_core::traits::OcrProvider;
use gh_core::{CoreError, CoreResult};
use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::HSTRING;

static PROCESS_MTA_ANCHOR: OnceLock<()> = OnceLock::new();

fn anchor_process_mta() {
    PROCESS_MTA_ANCHOR.get_or_init(|| {
        // SAFETY: Process-lifetime MTA usage increment (`CO_MTA_USAGE_COOKIE` intentionally kept for
        // process exit). `windows-rs`'s global `FactoryCache` holds `'static` activation-factory
        // pointers that become dangling if worker-thread `RoUninitialize()` drops MTA refcount to 0.
        let _ = unsafe { CoIncrementMTAUsage() };
    });
}

struct WinRtApartmentGuard {
    should_uninit: bool,
}

impl WinRtApartmentGuard {
    fn init() -> CoreResult<Self> {
        // SAFETY: Initializes WinRT MTA for the calling thread; `RPC_E_CHANGED_MODE` means
        // the thread is already initialized (e.g., STA) and must not be uninitialized by us.
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self {
                should_uninit: true,
            }),
            Err(err) if err.code() == RPC_E_CHANGED_MODE => Ok(Self {
                should_uninit: false,
            }),
            Err(err) => Err(CoreError::Io(std::io::Error::other(format!(
                "RoInitialize failed: {err}"
            )))),
        }
    }
}

impl Drop for WinRtApartmentGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Paired 1:1 with a successful `RoInitialize` on this thread.
            unsafe { RoUninitialize() };
        }
    }
}

thread_local! {
    static WINRT_APARTMENT: RefCell<Option<WinRtApartmentGuard>> = const { RefCell::new(None) };
}

/// Ensures WinRT is initialized on the calling thread with an RAII `RoUninitialize` guard.
pub fn ensure_winrt_initialized() -> CoreResult<()> {
    anchor_process_mta();
    WINRT_APARTMENT.with(|cell| {
        if cell.borrow().is_none() {
            let guard = WinRtApartmentGuard::init()?;
            *cell.borrow_mut() = Some(guard);
        }
        Ok(())
    })
}

fn winrt_err(context: &str, err: windows::core::Error) -> CoreError {
    CoreError::Io(std::io::Error::other(format!("{context}: {err}")))
}

/// Normalizes `OcrResult.Text` identically to `src/win_ocr.py`:
/// replaces `\r` and `\n` with `' '` and trims leading/trailing whitespace.
pub fn normalize_ocr_text(raw: &str) -> String {
    raw.replace(['\r', '\n'], " ").trim().to_owned()
}

/// Validates frame dimensions/stride/buffer and packs any `FrameView` format (`Bgr8`, `Bgra8`,
/// `Gray8`, `Rgba8`) into tightly packed `Bgra8` bytes for `SoftwareBitmap`.
pub fn pack_to_bgra8<'a>(frame: FrameView<'a>, max_dim: u32) -> CoreResult<Cow<'a, [u8]>> {
    if frame.width == 0 || frame.height == 0 {
        return Ok(Cow::Borrowed(&[]));
    }
    if frame.width > max_dim || frame.height > max_dim {
        return Err(CoreError::InvalidArgument(format!(
            "frame dimensions {}x{} exceed OcrEngine::MaxImageDimension ({max_dim})",
            frame.width, frame.height
        )));
    }
    if i32::try_from(frame.width).is_err() || i32::try_from(frame.height).is_err() {
        return Err(CoreError::InvalidArgument(format!(
            "frame dimensions {}x{} exceed i32 range",
            frame.width, frame.height
        )));
    }

    let w = frame.width as usize;
    let h = frame.height as usize;
    let channels = frame.format.channels();
    let min_row_bytes = w
        .checked_mul(channels)
        .ok_or_else(|| CoreError::InvalidArgument("row byte size overflow".into()))?;
    if frame.stride < min_row_bytes {
        return Err(CoreError::InvalidArgument(format!(
            "stride {} smaller than minimum row bytes {min_row_bytes}",
            frame.stride
        )));
    }
    let required_bytes = frame
        .stride
        .checked_mul(h - 1)
        .and_then(|v| v.checked_add(min_row_bytes))
        .ok_or_else(|| CoreError::InvalidArgument("frame buffer size overflow".into()))?;
    if frame.data.len() < required_bytes {
        return Err(CoreError::InvalidArgument(format!(
            "frame buffer truncated: got {} bytes, expected at least {required_bytes}",
            frame.data.len()
        )));
    }

    if frame.format == PixelFormat::Bgra8 && frame.stride == min_row_bytes {
        let total = min_row_bytes
            .checked_mul(h)
            .ok_or_else(|| CoreError::InvalidArgument("Bgra8 buffer size overflow".into()))?;
        return Ok(Cow::Borrowed(&frame.data[..total]));
    }

    let out_len = w
        .checked_mul(h)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| CoreError::InvalidArgument("output Bgra8 size overflow".into()))?;
    let mut out = Vec::with_capacity(out_len);

    for row_idx in 0..h {
        let row_start = row_idx * frame.stride;
        let row = &frame.data[row_start..row_start + min_row_bytes];
        match frame.format {
            PixelFormat::Bgra8 => out.extend_from_slice(row),
            PixelFormat::Bgr8 => {
                for px in row.as_chunks::<3>().0 {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            PixelFormat::Rgba8 => {
                for px in row.as_chunks::<4>().0 {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            PixelFormat::Gray8 => {
                for &g in row {
                    out.extend_from_slice(&[g, g, g, 255]);
                }
            }
        }
    }

    Ok(Cow::Owned(out))
}

fn create_ocr_engine() -> CoreResult<OcrEngine> {
    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }

    let langs = OcrEngine::AvailableRecognizerLanguages()
        .map_err(|e| winrt_err("AvailableRecognizerLanguages failed", e))?;
    let count = langs
        .Size()
        .map_err(|e| winrt_err("AvailableRecognizerLanguages::Size failed", e))?;

    let mut available: Vec<(String, Language)> = Vec::with_capacity(count as usize);
    for i in 0..count {
        if let Ok(lang) = langs.GetAt(i) {
            let tag = lang
                .LanguageTag()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            available.push((tag, lang));
        }
    }

    // Prefer `en-US`, then any `en*`, then first available recognizer language.
    let mut ordered_indices: Vec<usize> = Vec::with_capacity(available.len());
    for (idx, (tag, _)) in available.iter().enumerate() {
        if tag.eq_ignore_ascii_case("en-us") {
            ordered_indices.push(idx);
        }
    }
    for (idx, (tag, _)) in available.iter().enumerate() {
        let lower = tag.to_ascii_lowercase();
        if (lower == "en" || lower.starts_with("en-")) && !ordered_indices.contains(&idx) {
            ordered_indices.push(idx);
        }
    }
    for idx in 0..available.len() {
        if !ordered_indices.contains(&idx) {
            ordered_indices.push(idx);
        }
    }

    for idx in ordered_indices {
        let (_, lang) = &available[idx];
        if let Ok(engine) = OcrEngine::TryCreateFromLanguage(lang) {
            return Ok(engine);
        }
    }

    Err(CoreError::NotFound(
        "no Windows OCR recognizer language pack available".into(),
    ))
}

/// In-process WinRT `Windows.Media.Ocr.OcrEngine` service implementing `gh_core::traits::OcrProvider`.
///
/// Serializes `RecognizeAsync` calls with an internal `Mutex` because WinRT `OcrEngine` rejects
/// concurrent `RecognizeAsync` operations on the same instance (`0x80004004`).
pub struct WindowsOcrService {
    engine: Mutex<OcrEngine>,
    max_dim: u32,
    language_tag: String,
}

impl std::fmt::Debug for WindowsOcrService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsOcrService")
            .field("language_tag", &self.language_tag)
            .field("max_dim", &self.max_dim)
            .finish()
    }
}

impl WindowsOcrService {
    pub fn try_new() -> CoreResult<Self> {
        ensure_winrt_initialized()?;
        let engine = create_ocr_engine()?;
        let max_dim = OcrEngine::MaxImageDimension().unwrap_or(10_000);
        let language_tag = engine
            .RecognizerLanguage()
            .and_then(|l| l.LanguageTag())
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        Ok(Self {
            engine: Mutex::new(engine),
            max_dim,
            language_tag,
        })
    }

    pub fn new() -> CoreResult<Self> {
        Self::try_new()
    }

    /// Creates a service bound explicitly to a BCP-47 tag (e.g. `"en-US"`), useful for diagnostics/tests.
    pub fn try_with_language(tag: &str) -> CoreResult<Self> {
        ensure_winrt_initialized()?;
        let lang = Language::CreateLanguage(&HSTRING::from(tag))
            .map_err(|e| winrt_err("Language::CreateLanguage failed", e))?;
        let engine = OcrEngine::TryCreateFromLanguage(&lang)
            .map_err(|e| winrt_err("OcrEngine::TryCreateFromLanguage failed", e))?;
        let max_dim = OcrEngine::MaxImageDimension().unwrap_or(10_000);
        let language_tag = engine
            .RecognizerLanguage()
            .and_then(|l| l.LanguageTag())
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        Ok(Self {
            engine: Mutex::new(engine),
            max_dim,
            language_tag,
        })
    }

    pub fn recognizer_language_tag(&self) -> &str {
        &self.language_tag
    }

    pub const fn max_image_dimension(&self) -> u32 {
        self.max_dim
    }
}

impl OcrProvider for WindowsOcrService {
    fn recognize_text(&self, frame: FrameView<'_>) -> CoreResult<String> {
        if frame.width == 0 || frame.height == 0 {
            return Ok(String::new());
        }
        ensure_winrt_initialized()?;

        let bgra = pack_to_bgra8(frame, self.max_dim)?;
        let buffer = CryptographicBuffer::CreateFromByteArray(&bgra)
            .map_err(|e| winrt_err("CryptographicBuffer::CreateFromByteArray failed", e))?;
        let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &buffer,
            BitmapPixelFormat::Bgra8,
            frame.width as i32,
            frame.height as i32,
            BitmapAlphaMode::Ignore,
        )
        .map_err(|e| winrt_err("SoftwareBitmap::CreateCopyWithAlphaFromBuffer failed", e))?;

        let result = {
            let engine = self
                .engine
                .lock()
                .map_err(|_| CoreError::Sync("WindowsOcrService engine lock poisoned".into()))?;
            engine
                .RecognizeAsync(&bitmap)
                .map_err(|e| winrt_err("OcrEngine::RecognizeAsync failed", e))?
                .join()
                .map_err(|e| winrt_err("OcrEngine::RecognizeAsync join failed", e))?
        };

        let text = result
            .Text()
            .map_err(|e| winrt_err("OcrResult::Text failed", e))?
            .to_string_lossy();
        Ok(normalize_ocr_text(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::frame::FrameBuf;
    use std::path::{Path, PathBuf};
    use windows::Graphics::Imaging::BitmapDecoder;
    use windows::Storage::Streams::{Buffer, InMemoryRandomAccessStream};
    use windows::core::Array;

    fn decode_png_to_frame(png_bytes: &[u8]) -> CoreResult<FrameBuf> {
        ensure_winrt_initialized()?;
        let stream = InMemoryRandomAccessStream::new()
            .map_err(|e| winrt_err("InMemoryRandomAccessStream::new", e))?;
        let input_buf = CryptographicBuffer::CreateFromByteArray(png_bytes)
            .map_err(|e| winrt_err("CreateFromByteArray", e))?;
        let _ = stream
            .WriteAsync(&input_buf)
            .map_err(|e| winrt_err("WriteAsync", e))?
            .join()
            .map_err(|e| winrt_err("WriteAsync::join", e))?;
        stream.Seek(0).map_err(|e| winrt_err("Seek", e))?;

        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| winrt_err("BitmapDecoder::CreateAsync", e))?
            .join()
            .map_err(|e| winrt_err("BitmapDecoder::CreateAsync join", e))?;
        let sb = decoder
            .GetSoftwareBitmapConvertedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Ignore)
            .map_err(|e| winrt_err("GetSoftwareBitmapConvertedAsync", e))?
            .join()
            .map_err(|e| winrt_err("GetSoftwareBitmapConvertedAsync join", e))?;

        let w = sb.PixelWidth().map_err(|e| winrt_err("PixelWidth", e))? as u32;
        let h = sb.PixelHeight().map_err(|e| winrt_err("PixelHeight", e))? as u32;
        let out_buf = Buffer::Create(w * h * 4).map_err(|e| winrt_err("Buffer::Create", e))?;
        sb.CopyToBuffer(&out_buf)
            .map_err(|e| winrt_err("CopyToBuffer", e))?;

        let mut arr = Array::<u8>::new();
        CryptographicBuffer::CopyToByteArray(&out_buf, &mut arr)
            .map_err(|e| winrt_err("CopyToByteArray", e))?;

        Ok(FrameBuf {
            width: w,
            height: h,
            stride: (w as usize) * 4,
            format: PixelFormat::Bgra8,
            data: arr.as_slice().to_vec(),
        })
    }

    fn workspace_scratch_dir() -> Option<PathBuf> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.parent()?.parent()?.parent()?;
        Some(root.join("scratch"))
    }

    fn crop_region_roi(frame: &FrameBuf) -> FrameBuf {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let x1 = ((w as f64) * 0.80) as usize;
        let x2 = (((w as f64) * 0.96) as usize).min(w);
        let y1 = ((h as f64) * 0.88) as usize;
        let y2 = (((h as f64) * 0.96) as usize).min(h);
        let cw = x2.saturating_sub(x1);
        let ch = y2.saturating_sub(y1);
        let bpp = frame.format.channels();
        let stride = cw * bpp;
        let mut data = Vec::with_capacity(ch * stride);
        for row in y1..y2 {
            let start = row * frame.stride + x1 * bpp;
            data.extend_from_slice(&frame.data[start..start + stride]);
        }
        FrameBuf {
            width: cw as u32,
            height: ch as u32,
            stride,
            format: frame.format,
            data,
        }
    }

    fn recognize_via_winrt_native_decoder(png_bytes: &[u8], lang_tag: &str) -> CoreResult<String> {
        ensure_winrt_initialized()?;
        let stream = InMemoryRandomAccessStream::new()
            .map_err(|e| winrt_err("InMemoryRandomAccessStream::new", e))?;
        let input_buf = CryptographicBuffer::CreateFromByteArray(png_bytes)
            .map_err(|e| winrt_err("CreateFromByteArray", e))?;
        let _ = stream
            .WriteAsync(&input_buf)
            .map_err(|e| winrt_err("WriteAsync", e))?
            .join()
            .map_err(|e| winrt_err("WriteAsync::join", e))?;
        stream.Seek(0).map_err(|e| winrt_err("Seek", e))?;

        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| winrt_err("BitmapDecoder::CreateAsync", e))?
            .join()
            .map_err(|e| winrt_err("BitmapDecoder::CreateAsync join", e))?;
        let sb = decoder
            .GetSoftwareBitmapAsync()
            .map_err(|e| winrt_err("GetSoftwareBitmapAsync", e))?
            .join()
            .map_err(|e| winrt_err("GetSoftwareBitmapAsync join", e))?;

        let lang = Language::CreateLanguage(&HSTRING::from(lang_tag))
            .map_err(|e| winrt_err("Language::CreateLanguage", e))?;
        let engine = OcrEngine::TryCreateFromLanguage(&lang)
            .map_err(|e| winrt_err("OcrEngine::TryCreateFromLanguage", e))?;
        let result = engine
            .RecognizeAsync(&sb)
            .map_err(|e| winrt_err("OcrEngine::RecognizeAsync", e))?
            .join()
            .map_err(|e| winrt_err("OcrEngine::RecognizeAsync join", e))?;
        let text = result
            .Text()
            .map_err(|e| winrt_err("OcrResult::Text", e))?
            .to_string_lossy();
        Ok(normalize_ocr_text(&text))
    }

    fn query_persistent_ocr_ps1(ps1_path: &Path, image_paths: &[PathBuf]) -> Option<Vec<String>> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        if !ps1_path.is_file() || image_paths.is_empty() {
            return None;
        }
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "[Console]::InputEncoding=[System.Text.UTF8Encoding]::new($false); [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); & $env:GH_PS_OCR_PATH",
            ])
            .env("GH_PS_OCR_PATH", ps1_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        if let Some(mut stdin) = child.stdin.take() {
            for p in image_paths {
                let _ = writeln!(stdin, "{}", p.display());
            }
            let _ = writeln!(stdin, "QUIT");
        }

        let output = child.wait_with_output().ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let results: Vec<String> = stdout
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim_end_matches('\r');
                trimmed
                    .strip_prefix("RESULT:")
                    .map(normalize_ocr_text)
            })
            .collect();
        if results.len() == image_paths.len() {
            Some(results)
        } else {
            None
        }
    }

    /// Mirrors `src/locator.py::_normalize_region_text` accent/OCR-quirk folding for fixture assertions.
    fn fold_ocr_region_text(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        for ch in raw.to_lowercase().chars() {
            let mapped = match ch {
                'a' | 'à' | 'á' | 'ả' | 'ã' | 'ạ' | 'ă' | 'ằ' | 'ắ' | 'ẳ' | 'ẵ' | 'ặ' | 'â'
                | 'ầ' | 'ấ' | 'ẩ' | 'ẫ' | 'ậ' | 'ä' | 'å' => 'a',
                'e' | 'è' | 'é' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ề' | 'ế' | 'ể' | 'ễ' | 'ệ' | 'ë' => {
                    'e'
                }
                'i' | 'ì' | 'í' | 'ỉ' | 'ĩ' | 'ị' => 'i',
                'o' | 'ò' | 'ó' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ồ' | 'ố' | 'ổ' | 'ỗ' | 'ộ' | 'ơ'
                | 'ờ' | 'ớ' | 'ở' | 'ỡ' | 'ợ' | 'ö' | 'ø' => 'o',
                'u' | 'ù' | 'ú' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ừ' | 'ứ' | 'ử' | 'ữ' | 'ự' | 'ü' => {
                    'u'
                }
                'y' | 'ỳ' | 'ý' | 'ỷ' | 'ỹ' | 'ỵ' => 'y',
                'đ' => 'd',
                c if c.is_ascii_alphanumeric() => c,
                ' ' | '\t' => ' ',
                _ => continue,
            };
            out.push(mapped);
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn normalize_ocr_text_replaces_newlines_and_trims() {
        assert_eq!(normalize_ocr_text("  Nüi\r\nThånh\nVién\r "), "Nüi  Thånh Vién");
        assert_eq!(normalize_ocr_text("\r\n\t "), "");
    }

    #[test]
    fn pack_to_bgra8_handles_all_formats_and_padded_stride() {
        // 2x1 pixel in all 4 formats -> identical [B, G, R, 255] output
        let expected = vec![10u8, 20, 30, 255, 40, 50, 60, 255];

        let bgra = FrameView {
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Bgra8,
            data: &expected,
        };
        assert_eq!(pack_to_bgra8(bgra, 10_000).unwrap().as_ref(), expected.as_slice());

        // Bgr8 with stride padding (2*3 = 6 bytes + 2 padding bytes = 8)
        let bgr_padded = [10u8, 20, 30, 40, 50, 60, 0xAA, 0xBB];
        let bgr = FrameView {
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Bgr8,
            data: &bgr_padded,
        };
        assert_eq!(pack_to_bgra8(bgr, 10_000).unwrap().as_ref(), expected.as_slice());

        // Rgba8 (R,G,B,A) -> Bgra8 (B,G,R,A)
        let rgba_bytes = [30u8, 20, 10, 255, 60, 50, 40, 255];
        let rgba = FrameView {
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Rgba8,
            data: &rgba_bytes,
        };
        assert_eq!(pack_to_bgra8(rgba, 10_000).unwrap().as_ref(), expected.as_slice());

        // Gray8 with 2x2 and stride=4 (2 pixel bytes + 2 padding bytes per row)
        let gray_bytes = [100u8, 150, 0xFF, 0xFF, 200, 250, 0xFF, 0xFF];
        let gray = FrameView {
            width: 2,
            height: 2,
            stride: 4,
            format: PixelFormat::Gray8,
            data: &gray_bytes,
        };
        assert_eq!(
            pack_to_bgra8(gray, 10_000).unwrap().as_ref(),
            &[
                100, 100, 100, 255, 150, 150, 150, 255, 200, 200, 200, 255, 250, 250, 250, 255
            ]
        );
    }

    #[test]
    fn pack_to_bgra8_rejects_invalid_stride_truncated_and_oversized() {
        let bad_stride = FrameView {
            width: 4,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra8,
            data: &[0u8; 32],
        };
        assert!(matches!(
            pack_to_bgra8(bad_stride, 10_000),
            Err(CoreError::InvalidArgument(_))
        ));

        let truncated = FrameView {
            width: 4,
            height: 2,
            stride: 16,
            format: PixelFormat::Bgra8,
            data: &[0u8; 31],
        };
        assert!(matches!(
            pack_to_bgra8(truncated, 10_000),
            Err(CoreError::InvalidArgument(_))
        ));

        let oversized = FrameView {
            width: 10_001,
            height: 1,
            stride: 40_004,
            format: PixelFormat::Bgra8,
            data: &[],
        };
        assert!(matches!(
            pack_to_bgra8(oversized, 10_000),
            Err(CoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn empty_frame_returns_empty_string_and_multi_thread_ocr_works() {
        let Ok(svc) = WindowsOcrService::try_new() else {
            return;
        };
        let empty = FrameBuf::new_zeroed(0, 0, PixelFormat::Bgra8);
        assert_eq!(svc.recognize_text(empty.as_view()).unwrap(), "");

        // Verify Send + Sync across freshly spawned threads (WinRT MTA init per thread).
        let svc = std::sync::Arc::new(svc);
        let blank = FrameBuf::new_zeroed(64, 32, PixelFormat::Bgr8);
        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = std::sync::Arc::clone(&svc);
            let b = blank.clone();
            handles.push(std::thread::spawn(move || s.recognize_text(b.as_view())));
        }
        for h in handles {
            let res = h.join().expect("OCR thread panicked").unwrap();
            assert_eq!(res, "");
        }
    }

    #[test]
    fn ocr_query_fixtures_match_python_oracle() {
        let Some(scratch) = workspace_scratch_dir() else {
            return;
        };
        let Ok(svc) = WindowsOcrService::try_new() else {
            return;
        };
        assert!(svc.max_image_dimension() >= 1024);
        let lang_tag = svc.recognizer_language_tag().to_owned();
        assert!(!lang_tag.is_empty());

        // 1. Live parity on scratch/ocr_query_{0..5}.png against native WinRT decoder
        //    and read-only persistent_ocr.ps1 (if present), without assuming a fixed region name.
        let mut query_paths = Vec::new();
        let mut rust_outputs = Vec::new();
        for idx in 0..6 {
            let path = scratch.join(format!("ocr_query_{idx}.png"));
            if !path.is_file() {
                continue;
            }
            let png_bytes = std::fs::read(&path).expect("failed reading ocr_query fixture");
            let frame = decode_png_to_frame(&png_bytes).expect("WinRT PNG decode failed");
            let raw_text = svc
                .recognize_text(frame.as_view())
                .expect("WinRT OCR failed");
            let folded = fold_ocr_region_text(&raw_text);
            assert!(
                !folded.is_empty(),
                "expected non-empty OCR text on {}",
                path.display()
            );

            let native_raw = recognize_via_winrt_native_decoder(&png_bytes, &lang_tag)
                .expect("native WinRT decoder OCR failed");
            assert_eq!(
                raw_text, native_raw,
                "WindowsOcrService diverged from native WinRT BitmapDecoder on {}",
                path.display()
            );

            query_paths.push(path);
            rust_outputs.push(raw_text);
        }
        if !query_paths.is_empty() {
            assert_eq!(query_paths.len(), 6);
            let ps1_path = scratch.join("persistent_ocr.ps1");
            if ps1_path.is_file() {
                let ps_outputs = query_persistent_ocr_ps1(&ps1_path, &query_paths)
                    .expect("persistent_ocr.ps1 oracle returned mismatched output count");
                for (path, (rust_raw, ps_raw)) in
                    query_paths.iter().zip(rust_outputs.iter().zip(ps_outputs.iter()))
                {
                    assert_eq!(
                        rust_raw, ps_raw,
                        "Rust WindowsOcrService diverged from persistent_ocr.ps1 on {}",
                        path.display()
                    );
                }
            }
        }

        // 2. Deterministic hardcoded assertions on immutable in-memory ROIs cropped from
        //    scratch/diagnostics/*/01_game.png ([h*0.88..h*0.96, w*0.80..w*0.96]).
        let is_en = lang_tag.eq_ignore_ascii_case("en-us");
        let is_vi = lang_tag.eq_ignore_ascii_case("vi")
            || lang_tag.to_ascii_lowercase().starts_with("vi-");

        let diag_cases = [
            (
                scratch.join("diagnostics/diag_20261120_232232/01_game.png"),
                "lulhkong vu lua",
                "lung long vu lua",
            ),
            (
                scratch.join("diagnostics/diag_20261121_011322/01_game.png"),
                "i dai sanh fontaine",
                "dai sanh fontaine",
            ),
        ];

        for (img_path, expected_en, expected_vi) in diag_cases {
            if !img_path.is_file() {
                continue;
            }
            let png_bytes = std::fs::read(&img_path).expect("failed reading diagnostic image");
            let full = decode_png_to_frame(&png_bytes).expect("WinRT PNG decode failed");
            let roi = crop_region_roi(&full);
            let raw_text = svc
                .recognize_text(roi.as_view())
                .expect("WinRT ROI OCR failed");
            let folded = fold_ocr_region_text(&raw_text);
            if is_en {
                assert_eq!(
                    folded, expected_en,
                    "unexpected en-US ROI OCR on {}: raw={raw_text:?}",
                    img_path.display()
                );
            } else if is_vi {
                assert_eq!(
                    folded, expected_vi,
                    "unexpected vi ROI OCR on {}: raw={raw_text:?}",
                    img_path.display()
                );
            } else {
                assert!(
                    !folded.is_empty(),
                    "unexpected empty ROI OCR on {}",
                    img_path.display()
                );
                if let Ok(en_svc) = WindowsOcrService::try_with_language("en-US") {
                    let en_raw = en_svc
                        .recognize_text(roi.as_view())
                        .expect("WinRT en-US ROI OCR failed");
                    assert_eq!(
                        fold_ocr_region_text(&en_raw),
                        expected_en,
                        "unexpected fallback en-US ROI OCR on {}: raw={en_raw:?}",
                        img_path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn scoped_worker_threads_do_not_invalidate_winrt_factory_cache() {
        // Initialize WinRT ONLY inside scoped worker threads first, let those threads exit
        // (dropping their thread-local RoUninitialize guards), and then create/use WinRT OCR
        // on the main thread (nextest isolates each test into its own process).
        let blank = FrameBuf::new_zeroed(64, 32, PixelFormat::Bgr8);
        let mut any_available = false;
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                handles.push(scope.spawn(|| {
                    if let Ok(svc) = WindowsOcrService::try_new() {
                        let _ = svc.recognize_text(blank.as_view()).unwrap();
                        return true;
                    }
                    false
                }));
            }
            for h in handles {
                any_available |= h.join().expect("OCR worker panicked");
            }
        });
        if !any_available {
            return;
        }
        let main_svc = WindowsOcrService::try_new().expect("main-thread WinRT OCR after worker exit");
        assert_eq!(main_svc.recognize_text(blank.as_view()).unwrap(), "");
    }
}
