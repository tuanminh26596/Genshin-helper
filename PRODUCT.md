# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

- Primary users: Windows 10/11 (64-bit) PC Genshin Impact exploration and farming players (both Vietnamese and international English communities) who track Oculi, Chests, and Puzzles with the official HoYoLAB Interactive Map.
- Situation: Mid-session gaming (frequently at night or in dim rooms) on single-monitor or dual-monitor PC setups, opening the browser toolbar popup for 2–3 seconds to toggle engine state, check sync telemetry, adjust Mini-HUD or audio volume, trigger tools, or switch hotkeys without losing their HoYoLAB Map tab.

## Product Purpose

Genshin-Helper is a local-only, 100% passive Computer Vision companion that synchronizes real-time player position, underground floor layer, and 360° camera heading onto the official HoYoLAB Interactive Map while projecting an in-game click-through Mini-HUD and proximity Guide Card. Success on the Extension Control Center surface means every routine control is immediately scannable and operable from the toolbar popup in one click without vertical scrolling or switching away from the map tab, while keeping voluntary support links visible and accessible.

## Positioning

Strictly 100% passive Computer Vision architecture: zero game memory reads/writes, zero DLL injection, zero DirectX hooks, zero synthetic `SendInput` keystrokes against the game, and zero HoYoLAB credential/cookie collection—paired with 60 Hz minimap camera tracking, lossless multi-tab web sync, `WH_KEYBOARD_LL` hotkey conflict suppression, and opt-in NVIDIA Overlay remapping with 1-click backup restore.

## Operating Context

- Runs as a Manifest V3 browser extension (Chrome, Edge, Brave) talking strictly to the local pure-Rust desktop companion (`GenshinHelper.exe` + `GenshinHelperHost.exe`, 7-crate Cargo workspace in `rust/`) over `http://127.0.0.1:8765` and Native Messaging (`com.genshinhelper.host`).
- Browser popup sits directly above the HoYoLAB Interactive Map (`https://act.hoyolab.com/ys/app/interactive-map/*`); Chromium caps popup height at 600 px while permitting up to 800 px width.
- Operated alongside Genshin Impact in Borderless Windowed mode on 1080p/1440p monitors, often in low-light gaming environments (dark surface context).
- Bilingual runtime switching (`VI | EN`) synchronized live between the Extension Popup, injected map scripts, System Tray, and in-game Win32 Mini-HUD (`tiny-skia` + `cosmic-text`).

## Capabilities and Constraints

- Surface: Keep the toolbar popup (`extension/popup.html`, `extension/popup.css`, `extension/popup.js`) at ~520 px wide with a 2-column command-deck layout across the 3 existing tabs (`Control` / `Tools` / `Hotkeys`), zero vertical scroll on Tab 1 and Tab 2 (`height <= 580 px`), and a sticky bottom footer keeping connection status and the voluntary Donate trigger visible at all times.
- Offline & CSP constraints: Vanilla HTML/CSS/JS MV3 extension; must function 100% offline inside the extension bundle with zero external CDN requests, zero `eval()`, and no new permissions in `extension/manifest.json` (preserving the pinned RSA key / Extension ID `nfkijdoblmphmdljfogooajhbldkeojh`).
- Existing contract & test invariants: Preserve all existing interactive element IDs required by `tests/verify_i18n_and_ext_v5.py` and `extension/popup.js`, keep strict `extension/locales/vi.json` ↔ `extension/locales/en.json` key and placeholder parity, and preserve all 3 tabs and desktop API endpoints (`/status`, `/set_engine_state`, `/set_auto_sync`, `/set_minimap_tracking`, `/set_nav_hud`, `/set_audio`, `/set_single_monitor`, `/set_language`, `/set_hotkeys`, `/check_hotkey_conflicts`, `/remap_external_hotkey`, `/restore_external_hotkey`, `/lan_companion`, `/launch_borderless`, `/trigger_diagnostic`, `/quick_switch`, `/shutdown`).

## Brand Commitments

- Name: **Genshin Helper** (`v5.8.0`), 100% free and open-source (MIT) with zero ads; support (`Ko-fi` and `MoMo / VietQR`) is strictly voluntary and framed respectfully.
- Existing official assets to preserve: original compass icon (`extension/icons/icon48.png`) and MoMo VietQR image (`extension/icons/momo_qr.jpg`).
- Dark-surface operating context confirmed for gaming use alongside Genshin Impact and HoYoLAB Interactive Map.

## Evidence on Hand

- Extension source: `extension/popup.html`, `extension/popup.css`, `extension/popup.js`, `extension/background.js`, `extension/i18n.js`, `extension/i18n_bridge.js`, `extension/locales/vi.json`, `extension/locales/en.json`.
- Pure-Rust backend (`rust/crates/gh-{core,win32,net,cv,hud,host,app}`): measured release footprint with `0` external DLLs (`~7.8 MB` Inno Setup installer), `< 0.4 ms` pre-baked Mini-HUD rendering, `< 3 ms/frame` `heading360` tracking, and 60 Hz heading / 20 Hz full minimap loop (`0%` CPU in Standby).
- Official icons and assets: `extension/icons/icon16.png`, `icon32.png`, `icon48.png`, `icon128.png`, `extension/icons/momo_qr.jpg`.
- Verification suites: `cargo nextest run --manifest-path rust/Cargo.toml --workspace`, `tests/test_blackbox_contracts.py --backend rust`, `tests/verify_i18n_and_ext_v5.py`, `scripts/audit_release.py --strict-ip`.
- No fabricated metrics, testimonials, or unverified claims permitted.

## Product Principles

1. **Zero-friction mid-game control:** Every frequent toggle and status indicator must be reachable in one click from the toolbar popup without switching tabs or scrolling.
2. **Passive transparency and trust:** Communicate engine state (`Online` / `Standby` / `Offline`), UIPI elevation warnings, hotkey conflicts, and external remaps honestly and recoverably.
3. **Bilingual equality (`VI | EN`):** Both Vietnamese and English strings must fit cleanly without truncation, clipping, or layout breakage.
4. **Honest voluntary support:** Keep the voluntary Donate trigger unobtrusively visible in the persistent footer without blocking or nagging primary controls.

## Accessibility & Inclusion

- Keyboard-accessible tablist (`role="tablist"`), switches, hotkey recorders (`Esc` to cancel), and focus-visible rings across all interactive controls.
- Full bilingual `VI | EN` UI switching with legible contrast for dark-room gaming.
