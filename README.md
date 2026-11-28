<div align="center">
  <img src="extension/icons/icon128.png" alt="Genshin-Helper Icon" width="92" height="92" />
  <h1>Genshin-Helper v5.8.0</h1>
  <p><strong>100% Passive Computer Vision Map Sync, 60 fps Live Radar &amp; 360° Navigation Overlay for HoYoLAB Interactive Map</strong></p>
  <p>
    <strong>English</strong> | <a href="README.vi.md">Tiếng Việt</a>
  </p>
  <p>
    <a href="https://github.com/tuanminh26596/Genshin-helper/releases"><img src="https://img.shields.io/badge/release-v5.8.0-38bdf8?style=flat-square" alt="Release v5.8.0"></a>
    <a href="https://github.com/tuanminh26596/Genshin-helper/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/tuanminh26596/Genshin-helper/ci.yml?branch=main&label=CI&style=flat-square" alt="CI"></a>
    <img src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0ea5e9?style=flat-square" alt="Windows 10/11 x64">
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-22c55e?style=flat-square" alt="MIT License"></a>
    <a href="https://discord.gg/DdA4qyvqH3"><img src="https://img.shields.io/badge/Discord-Join%20Community-5865F2?style=flat-square&logo=discord&logoColor=white" alt="Discord Community"></a>
    <a href="https://ko-fi.com/minmories"><img src="https://img.shields.io/badge/Ko--fi-minmories-FF5E5B?style=flat-square&logo=kofi&logoColor=white" alt="Ko-fi"></a>
  </p>
  <p>
    <img src="docs/media/demo-v57-en.gif" alt="Genshin-Helper v5.8.0 demo: 60 Hz Web Map Sync, M-Map Route Overlay, Live Radar + Guide Card, Mobile LAN Companion, and 100% Passive CV proof" width="800" />
    <br/>
    <sub><em>60 Hz Web Map Sync · M-Map Route Overlay · Live Radar &amp; Guide Card · Mobile LAN Companion · ~2% CPU 100% Passive CV</em></sub>
  </p>
  <p>
    <a href="https://github.com/tuanminh26596/Genshin-helper/releases/latest"><strong>⬇ Download Setup v5.8.0 (~8.5 MB)</strong></a>
    &nbsp;·&nbsp;
    <a href="#quick-start">Quick Start</a>
    &nbsp;·&nbsp;
    <a href="https://discord.gg/DdA4qyvqH3">Discord Support</a>
    &nbsp;·&nbsp;
    <a href="#support-the-project">☕ Support / Donate</a>
  </p>
</div>

---

## Key Features (`v5.8.0`)

**Genshin-Helper** is a local-only, **100% passive Computer Vision** companion for **Genshin Impact (Windows 10/11 x64)** — pure Rust (`0` external DLLs, `~2%` CPU, `0%` in Standby) that syncs your real-time position & 360° camera onto the official **HoYoLAB Interactive Map** without reading memory, injecting DLLs, or sending fake inputs.

- **Floating Live Radar (60 fps, ``Alt + ` ``) & Mode A Route Sync:** Default OFF (`0%` CPU/RAM). Official HoYoLAB pin icons, 360° camera cone, live filter sync, colored route polylines, and next-teleport highlight.
- **In-Game Map (`M`) Route Overlay & Capture-Ready HUDs:** Projects active HoYoLAB farming routes directly onto fullscreen Map (`M`), alongside the 360° click-through Mini-HUD and proximity Guide Card (`Alt + H`) with OBS / Screenshot / NVIDIA `Alt + F9` visibility toggle.
- **Automatic Windows HDR Capture (`DXGI` + `WGC`):** Auto-detects HDR displays (`IDXGIOutput6`), tonemaps 16-bit floating-point WGC frames cleanly, and falls back to zero-flicker GDI `SRCCOPY` on SDR monitors.
- **60 Hz Minimap Tracking & Underground Switching:** Hybrid SIFT + Wiener phase correlation + zero-mean Sobel with 0.5× structural Surface ↔ Underground floor detection and offline RootSIFT full-Teyvat relocation.
- **Mobile LAN Companion (`:8766`) & Smart Hotkeys:** Scan local Wi-Fi QR for 1:1 mobile map, route overlay, pin guide cards & quick mark; Shift/Ctrl-free left-hand `Alt` defaults + opt-in NVIDIA Overlay remapper (1-click backup restore).
- **SHA-256 Verified Auto-Updater & Zero-UAC Wake:** Automatic GitHub Release checks with FIPS 180-4 SHA-256 checksum verification (`SHA256SUMS.txt`) and elevated Scheduled Task (`GenshinHelper_Elevated`).

| In-Game Mini-HUD, Guide Card & Live Radar Route | 100% Passive CV Proof (~2% CPU · 181 MB RAM) |
| :---: | :---: |
| <img src="docs/media/v57-radar-guide-en.jpg" alt="In-game Mini-HUD, Guide Card, and Floating Live Radar with Route Overlay" width="100%" /><br/><sub>360° Mini-HUD, Guide Card (<code>Alt + H</code>) &amp; colored Route Path on Live Radar</sub> | <img src="docs/media/v57-cv-proof-en.jpg" alt="Task Manager proving ~2% CPU, 181 MB RAM, and passive CV working even on video playback" width="100%" /><br/><sub>Tracks even on recorded video playback with zero game process / memory access</sub> |

<details>
<summary><strong>📸 More screenshots: M-Map Route Overlay, Mobile LAN Companion &amp; Extension Control Center</strong></summary>
<br/>

| Route Overlay Drawn Directly on Map (`M`) | Mobile LAN Companion (Route + Pin Guides) |
| :---: | :---: |
| <img src="docs/media/v57-mmap-route-en.jpg" alt="HoYoLAB farming route overlaid directly onto fullscreen in-game Map M" width="100%" /><br/><sub>Active HoYoLAB farming route &amp; nearest teleport highlighted right on Map (<code>M</code>)</sub> | <img src="docs/media/v57-lan-mobile-en.jpg" alt="Mobile LAN Companion showing synced route map and interactive pin guide sheet" width="100%" /><br/><sub>Scan local Wi-Fi QR on phone/tablet for 1:1 live map, pin guides &amp; quick mark</sub> |

<p align="center">
  <img src="docs/media/extension-popup.jpg" alt="Clean HoYoLAB Interactive Map with Extension Control Center Popup" width="88%" />
  <br/>
  <sub><em>MV3 Extension Control Center Popup over a clutter-free HoYoLAB Interactive Map</em></sub>
</p>
</details>

---

## Quick Start

1. Download **`GenshinHelper-Setup-v5.8.0.exe`** (`~8.5 MB`, `0` external DLLs) from [**GitHub Releases**](https://github.com/tuanminh26596/Genshin-helper/releases/latest) and run as **Administrator** (required by Windows UIPI so hotkeys/HUD work over Genshin).
2. When Setup finishes, follow the auto-opened **Extension Setup Guide** (`install_extension.html`): open `chrome://extensions` (or Edge / Brave) → enable **Developer mode** → **Load unpacked** → paste the shown `extension` folder path & pin the icon.
3. On first launch, `GenshinHelper.exe` auto-downloads public HoYoLAB map metadata (~15 MB) into `%LOCALAPPDATA%\GenshinHelper\data` (~10–20s once).
4. Open [**HoYoLAB Interactive Map**](https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2), set Genshin Impact to **Borderless Windowed**, press **`M`** once to lock position, and explore!

<details>
<summary><strong>🛠️ Build from Source (Rust 1.92+ / Edition 2024), CLI Flags &amp; Config</strong></summary>
<br/>

Prerequisites: [Rustup (`msvc` stable)](https://rustup.rs/) and Visual Studio Build Tools (C++ workload).

```powershell
git clone https://github.com/tuanminh26596/Genshin-helper.git
cd Genshin-helper

Copy-Item config.example.json config.json
cargo build --manifest-path rust/Cargo.toml --release -p gh-app -p gh-host
# Optional pre-hydration (also runs automatically on first launch):
cargo run --manifest-path rust/Cargo.toml --release -p gh-app -- --sync-data
# Register dev Native Messaging host for unpacked extension/:
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/register_dev_native_host.ps1
```

Launch `GenshinHelper.exe` directly (auto-prompts UAC; pass `--no-elevate` or `GH_NO_ELEVATE=1` to skip; run `installer/build_release.ps1` with Inno Setup 6 to build `dist/installer/GenshinHelper-Setup-v5.8.0.exe`).

- **CLI Flags:** `--sync-data`, `--no-tray`, `--standby`, `--minimized`, `--record-minimap <SECONDS>`, `--port <PORT>`, `--lan-port <LAN_PORT>`, `--config <PATH>`, `--no-mutex`, `--no-elevate`.
- **`hoyolab_sync` (`config.json`):** Auto-checks HoYoLAB `map_version` on startup (`"auto_check_on_startup": true`, `"auto_update_mode": "auto_download"` | `"notify_only"` | `"off"`).

</details>

---

## Default Hotkeys (`v5.8.0`)

Shift/Ctrl-free left-hand `Alt` combos — fully customizable in **Extension Popup → Hotkeys** (with automatic conflict detection):

| Action | Default | Purpose |
| --- | --- | --- |
| **Mark / Undo HUD Target** | `Alt + F` / `Alt + T` | Marks active target on Desktop App & HoYoLAB Web Map (>25 m asks double-tap) / reverts last mark. |
| **Guide Card / Next Target** | `Alt + H` / `Alt + E` | Toggles HoYoLAB community tips & screenshot card / cycles to next nearby pin. |
| **Live Radar / Mini-HUD** | ``Alt + ` `` / `Alt + X` | Toggles 60 fps Floating Live Radar (default OFF) / shows or hides Mini-HUD. |
| **Edit Layout / Cancel Lock** | `Alt + F7` / `Alt + F6` | Drag/resize Mini-HUD & Radar layout / clears manually locked target. |
| **Sync Map (`M`) / Switch / Debug** | `Alt + F11` / `Alt + Q` / `Alt + F12` | Sync Map (`M`) & mark nearest (<60 m) / switch Game ↔ Map / privacy-blurred diagnostics. |

> **Recording Tip:** Enable **Show HUD in screenshots & video** in Popup Control tab for OBS / `Win+Shift+S` / Share Screen. For NVIDIA `Alt + F9`, also enable **Desktop Capture** in NVIDIA Overlay (`Alt + Z`) → Settings → Privacy Control.

---

## Anti-Cheat Safety, Privacy & Transparency

- **100% Passive Design — Honest Risk Notice:** Built and used daily by the author with **zero game memory reads/writes**, **zero DLL injection**, **zero DirectX hooks**, and **zero `SendInput`** sent to the game. Still, this is an **unofficial third-party tool** not affiliated with HoYoverse — nobody can guarantee 0% ban risk; **use at your own discretion and responsibility.**
- **Zero Login / Zero Telemetry:** Never reads HoYoLAB cookies or passwords; binds locally to `127.0.0.1:8765`; diagnostic bundles (`Alt + F12`) auto-blur the UID corner.
- **Open Build & SmartScreen:** Unsigned open-source binaries with global hotkeys (`WH_KEYBOARD_LL`) may trigger Windows SmartScreen. Official releases include `SHA256SUMS.txt` and can be audited or built cleanly from source.

<details>
<summary><strong>📋 Full Compatibility Matrix, Disclaimer Details &amp; System Architecture</strong></summary>
<br/>

| Setting / Environment | Status | Behavior & Recommended Setup |
| --- | --- | --- |
| **Borderless Windowed / Windowed** | Full support | Recommended for Mini-HUD, Live Radar, M-Map Route Overlay & single-monitor play. |
| **Exclusive Fullscreen** *(OS Limit)* | No overlay | Windows DWM blocks layered HUDs. Use the built-in **Launch Borderless** button (`-popupwindow -screen-fullscreen 0`). |
| **Admin / UIPI** *(Windows Limit)* | Matching Admin | Setup creates `GenshinHelper_Elevated` Scheduled Task for UAC-free wakes (cleanly removed on uninstall). |
| **Windows HDR / Auto-HDR** | Full support *(Auto WGC)* | Auto-detects HDR via DXGI (`IDXGIOutput6`), captures 16-bit float WGC with tonemapping, falls back to GDI `SRCCOPY` on SDR. |
| **Aspect Ratios / DPI / Input / Rotation** | Full support | Supports `16:9`, `16:10`, `21:9`, `4:3`, `100%–200%` DPI, KB+M & Gamepad, Lock & Follow-View minimap. |

- **NVIDIA Overlay Remap:** Opt-in only after explicit confirmation in Popup, with automatic backup and 1-click restore.
- **Scope & License:** Windows 10/11 64-bit PC client only; released under the [MIT License](LICENSE) **AS IS**.

```text
+----------------------------------+          localhost:8765 (HTTP)         +---------------------------------+
| Genshin-Helper Pure-Rust (Win64) | <====================================> | Chrome / Edge / Brave Extension |
|  GenshinHelper.exe (0 ext DLLs)  |   Lossless /poll?since=<seq> ring buf  |  (MV3 Service Worker + Popup)   |
|  - Passive WGC HDR / GDI SRCCOPY |                                        |  - HoYoLAB Map & Route Bridge   |
|  - Pure-Rust Map M + Minimap CV  |        Native Messaging (stdio)        |  - Zero-Throttle Background Sync|
|  - Mini-HUD, Live Radar & M-Map  | <------------------------------------> |  - TSP Overlay & Pin Exporter   |
|  - Local Cache (%LOCALAPPDATA%)  |    GenshinHelperHost.exe (pinned ID)   +---------------------------------+
+----------------------------------+
```

- **Community (Discord):** [Join Discord Server](https://discord.gg/DdA4qyvqH3)
- **Docs:** [extension/README.md](extension/README.md) · [CONTRIBUTING.md](CONTRIBUTING.md) · [SECURITY.md](SECURITY.md) · [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)

</details>

---

## Support the Project

Genshin-Helper is **100% free, open-source, and ad-free** — every feature stays unlocked forever. Updating map datasets, cave layers, and CV tracking for every new Genshin region (Nod-Krai, upcoming Snezhnaya) takes dedicated time. If it saves you hours exploring Teyvat, a voluntary coffee helps keep updates fast and sustainable:

| International Supporters | Supporters in Vietnam (MoMo / VietQR) |
| :---: | :---: |
| <a href="https://ko-fi.com/minmories"><img src="https://img.shields.io/badge/Support%20on%20Ko--fi-minmories-FF5E5B?style=for-the-badge&logo=kofi&logoColor=white" alt="Support on Ko-fi" /></a><br/><br/><strong>Ko-fi:</strong> <https://ko-fi.com/minmories><br/><strong>GitHub Sponsors:</strong> <code>@tuanminh26596</code> <em>(pending review)</em><br/><strong>Discord:</strong> <https://discord.gg/DdA4qyvqH3><br/><br/><sub>100% voluntary tip — thank you for keeping this project alive!</sub> | <img src="assets/donate/momo_qr.jpg" alt="MoMo VietQR Support" width="180" /><br/><sub>Scan with MoMo or any VN banking app</sub> |
