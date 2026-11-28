# Contributing to Genshin-Helper

Thank you for your interest in improving Genshin-Helper! Because this project interacts alongside a live game on Windows and is distributed publicly under the MIT License, every contribution must uphold our **anti-cheat safety**, **license/IP hygiene**, and **verification** invariants.

---

## 1. Development Environment (Windows 10/11 64-bit)

Prerequisites:
- **Rust toolchain:** `rustc >= 1.92` (`edition = "2024"`, `x86_64-pc-windows-msvc` target) with `clippy`, `rustfmt`, and [`cargo-nextest`](https://nexte.st/)
- **MSVC Build Tools:** Visual Studio 2022 C++ Desktop workload
- **Node.js 20+** (for Extension JS syntax/runtime checks) & **Python 3.12+** (only for stdlib-based release/audit/black-box tooling; `cv2`+`numpy` in `.venv` only needed for optional `cv-heavy` Python-oracle parity benchmarks)

```powershell
git clone https://github.com/tuanminh26596/Genshin-helper.git
cd Genshin-helper
Copy-Item config.example.json config.json
```

### 7-Crate Workspace Layout (`rust/crates/`)
- `gh-core` (Tier 0 leaf): config, paths, atomic I/O, `.npy`/`.npz`, i18n (`assets/locales/*.json`), `ui_layout`, `multimap`, `assets`, `nav_controller`, `tsp_route`, `map_bridge`, `protocol`, `traits`
- `gh-win32` (Tier 1): GDI `SRCCOPY` capture, `WH_KEYBOARD_LL`/`WH_MOUSE_LL` hotkeys, conflict advisor, game settings, WinRT OCR, tray, audio, window switcher, locale/mutex
- `gh-net` (Tier 1): Sync Server (`127.0.0.1:8765`), LAN Companion (`0.0.0.0:8766`), HoYoLAB sync, HD tiles/overlays, Pin Guide fetcher
- `gh-cv` (Tier 1): pure-Rust CV ops (Sobel, 6-FFT NCC, phaseCorrelate, warpPolar/warpAffine, contours, RootSIFT, 4-tree KD-Tree, ORB, RANSAC), `zoom_slider`, `map_state`, `heading360`, `tile_cache`, `global_relocator`, `locator`, `minimap_tracker`
- `gh-hud` (Tier 2): `WS_EX_LAYERED` HUD, `%WINDIR%\Fonts` (`cosmic-text` + `tiny-skia`), `NavHudRenderer`, toast, diagnostic annotation
- `gh-host`: standalone `GenshinHelperHost.exe` Chrome Native Messaging binary
- `gh-app`: `GenshinHelper.exe` main application binary

### Build Cache & Dataset Hydration
- Keep build artifacts out of `rust/target/` (`0` bytes in `rust/target/`). Configure `CARGO_TARGET_DIR` via root `/.cargo/config.toml` (gitignored, e.g. `C:\CargoTarget\Genshin-Helper`).
- Static non-IP assets live in `assets/` (`assets/data/category_aliases.json`, `assets/locales/{vi,en}.json`, `assets/zoom_thumb_template.png`, `assets/icons/*`).
- Hydrate local HoYoLAB dev datasets into `%LOCALAPPDATA%\GenshinHelper\data` (or gitignored `data/`) via:

```powershell
cargo run --manifest-path rust/Cargo.toml --release -p gh-app -- --sync-data
```

---

## 2. Non-Negotiable Engineering Invariants

1. **Passive Computer Vision Only (Anti-Cheat Safety):**
   - **NEVER** read/write game memory, inject DLLs, or hook game rendering pipelines.
   - **NEVER** call `SendInput` (or equivalent synthetic input APIs) against the game window.
2. **Preserve `DECISION:` Invariants:**
   - Do not modify or delete code annotated with `DECISION:` without understanding and preserving the documented invariant.
   - Keep `gh-cv::zoom_slider` / `gh-cv::map_state` dual-gate verification (`TM_CCORR_NORMED` + zero-mean `TM_CCOEFF_NORMED >= 0.65`).
   - Keep `gh-net::sync_server` `/poll?since=<seq>` lossless monotonic ring buffer intact.
   - Keep typed `windows` (`windows-rs`) Win32 API bindings in `gh-win32::{hotkey,capture,game_settings}` read-only (GDI `SRCCOPY` only, zero `SendInput`).
   - Keep the pinned RSA-2048 SPKI `"key"` in `extension/manifest.json` (ID `nfkijdoblmphmdljfogooajhbldkeojh`) synchronized with `installer/native_host_manifest.json` and `installer/GenshinHelper.iss`.
   - Keep `extension/**` and `scratch/**` offline fixtures intact.
3. **License & IP Hygiene (Public MIT Repository):**
   - Submit only original MIT-compatible code, or properly attributed ports from MIT/BSD/Apache-2.0 repositories with a top-of-file attribution header (`// Ported from <repo>@<commit>:<path> (MIT License)`) and an entry in `THIRD_PARTY_NOTICES.md`.
   - **NEVER** copy code from AGPL, GPL, or unlicensed ("all rights reserved") repositories.
   - **NEVER** commit copyrighted HoYoverse artwork, game icons, HoYoLAB map tiles, `data/*.npz`, personal cookies/tokens, `rust/target/` artifacts, or `scratch/` diagnostic dumps.

---

## 3. Required Pre-PR Checks

Run all checks locally before opening a Pull Request:

```powershell
# 1. Fast Gate (Clippy + Workspace Unit/Integration Tests)
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
cargo nextest run --manifest-path rust/Cargo.toml --workspace

# 2. Build Release EXEs & Run Black-Box Contract Suite + i18n/Extension Gate
cargo build --manifest-path rust/Cargo.toml --release -p gh-app -p gh-host
.\.venv\Scripts\python.exe tests/verify_i18n_and_ext_v5.py
$env:GH_TEST_HOST_COLD_LAUNCH = "1"; $env:GH_HOST_SKIP_SCHTASKS = "1"
.\.venv\Scripts\python.exe tests/test_blackbox_contracts.py --backend rust -v

# 3. Release Privacy, Cargo License & Strict IP Audit
.\.venv\Scripts\python.exe scripts/audit_release.py --strict-ip
.\.venv\Scripts\python.exe scripts/audit_release.py --dir rust --strict-ip
.\.venv\Scripts\python.exe scripts/collect_licenses.py --cargo-manifest rust/Cargo.toml --strict
```

When modifying `gh-cv` or CV benchmarks, also run the Heavy CV fixture gate (requires `scratch/` & `data/` fixtures + `.venv` `cv2`/`numpy`):

```powershell
cargo nextest run --manifest-path rust/Cargo.toml --workspace --profile cv-heavy --cargo-profile cv-bench --run-ignored all
```

---

## 4. Pull Request Guidelines

- Keep PRs focused, well-scoped, and accompanied by a deterministic offline verification check when adding non-trivial logic.
- Update both `README.md` (English) and `README.vi.md` (Vietnamese) when changing user-facing features, hotkeys, or compatibility behavior.
- Fill out `.github/PULL_REQUEST_TEMPLATE.md` honestly; never claim a test passed without running it.

---

## 5. Large PRs & AI-Assisted Contributions

- **Discuss Large Changes First:** Before writing large PRs (~400+ lines), adding new crates, or introducing new OS/platform support, open an Issue to align on scope and design. Prefer splitting big features into smaller, focused PRs. Any PR adding a new platform must include CI coverage for that platform while keeping existing Windows checks passing.
- **AI-Assisted PRs (Disclosure & Ownership):** AI tools and coding agents are welcome **only when** the author:
  1. **Discloses AI assistance** honestly in the PR template.
  2. **Owns every line:** has personally read, understood, and tested the diff, and can explain the design and respond to review feedback.
  3. **Verifies honestly:** never checks off tests that were not run (clearly note checks skipped due to host OS).
  4. **Stays strictly in scope:** avoids unrelated reformatting, lint suppressions, or out-of-scope edits; changes to high-risk areas (`gh-cv/`, `gh-win32/`, `gh-app/src/updater.rs`, `extension/manifest.json`) must include clear rationale and deterministic regression coverage.
- **Review Triage:** Oversized, unscoped, or unverified drive-by AI PRs may be closed without a full line-by-line review so the work can be resubmitted cleanly in smaller, verified steps.
