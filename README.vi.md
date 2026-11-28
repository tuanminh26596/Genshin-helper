<div align="center">
  <img src="extension/icons/icon128.png" alt="Biểu tượng Genshin-Helper" width="92" height="92" />
  <h1>Genshin-Helper v5.8.0</h1>
  <p><strong>Trợ lý Định vị Thị giác Máy tính Thụ động 100%, Bản đồ Radar 60 fps &amp; Mini-HUD Điều hướng 360° cho HoYoLAB Interactive Map</strong></p>
  <p>
    <a href="README.md">English</a> | <strong>Tiếng Việt</strong>
  </p>
  <p>
    <a href="https://github.com/tuanminh26596/Genshin-helper/releases"><img src="https://img.shields.io/badge/release-v5.8.0-38bdf8?style=flat-square" alt="Phiên bản v5.8.0"></a>
    <a href="https://github.com/tuanminh26596/Genshin-helper/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/tuanminh26596/Genshin-helper/ci.yml?branch=main&label=CI&style=flat-square" alt="CI"></a>
    <img src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0ea5e9?style=flat-square" alt="Windows 10/11 x64">
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-22c55e?style=flat-square" alt="Giấy phép MIT"></a>
    <a href="https://discord.gg/DdA4qyvqH3"><img src="https://img.shields.io/badge/Discord-C%E1%BB%99ng%20%C4%91%E1%BB%93ng-5865F2?style=flat-square&logo=discord&logoColor=white" alt="Cộng đồng Discord"></a>
    <a href="https://ko-fi.com/minmories"><img src="https://img.shields.io/badge/Ko--fi-minmories-FF5E5B?style=flat-square&logo=kofi&logoColor=white" alt="Ko-fi"></a>
  </p>
  <p>
    <img src="docs/media/demo-v57-vi.gif" alt="Demo Genshin-Helper v5.8.0: Đồng bộ Web Map 60 Hz, Tuyến đường trên Map M, Live Radar + Thẻ Hướng Dẫn, LAN Companion điện thoại và bằng chứng 100% CV" width="800" />
    <br/>
    <sub><em>Đồng bộ Web Map 60 Hz · Phủ tuyến lên Map (M) · Live Radar &amp; Thẻ Hướng Dẫn · LAN Companion Điện thoại · Chứng minh 100% CV (~2% CPU)</em></sub>
  </p>
  <p>
    <a href="https://github.com/tuanminh26596/Genshin-helper/releases/latest"><strong>⬇ Tải Bộ cài v5.8.0 (~8.5 MB)</strong></a>
    &nbsp;·&nbsp;
    <a href="#cài-đặt-nhanh">Cài đặt nhanh</a>
    &nbsp;·&nbsp;
    <a href="https://discord.gg/DdA4qyvqH3">Cộng đồng Discord</a>
    &nbsp;·&nbsp;
    <a href="#ủng-hộ-tác-giả-donate">☕ Ủng hộ / Donate</a>
  </p>
</div>

---

## Tính năng nổi bật (`v5.8.0`)

**Genshin-Helper** là công cụ đồng bộ bản đồ chạy cục bộ hoàn toàn bằng **Thị giác Máy tính (Computer Vision) thụ động 100%** cho **Genshin Impact (Windows 10/11 64-bit)** — viết thuần Rust (`0` DLL ngoài, `~2%` CPU, `0%` khi Standby), không đọc RAM, không inject DLL và không gửi phím giả vào game.

- **Bản đồ nổi Live Radar 60 fps (``Alt + ` ``) & Đồng bộ Tuyến (Mode A):** Mặc định TẮT (`0%` CPU/RAM). Icon ghim chính chủ HoYoLAB, nón camera 360°, đồng bộ bộ lọc ghim, vẽ đường nối theo chặng và làm sáng điểm dịch chuyển cần tới.
- **Phủ Tuyến đường lên Map (`M`) & HUD quay được OBS:** Vẽ trực tiếp tuyến đường HoYoLAB lên bản đồ toàn màn hình (`M`), đi kèm Mini-HUD 360° xuyên chuột và Thẻ Hướng Dẫn (`Alt + H`) có công tắc hiển thị khi chụp ảnh / quay OBS / NVIDIA `Alt + F9`.
- **Tự động hỗ trợ màn hình Windows HDR (`DXGI` + `WGC`):** Tự nhận diện màn HDR (`IDXGIOutput6`), tone-map khung hình 16-bit float qua WGC mượt mà và tự chuyển về GDI `SRCCOPY` trên màn SDR.
- **Theo dõi Minimap 60 Hz & Tự chuyển tầng Hang Ngầm:** Kết hợp SIFT + tương quan pha Wiener + Sobel trên tile HD, nhận diện tầng Mặt Đất ↔ Hang Ngầm 0.5× tổng quát và định vị lại toàn Teyvat bằng RootSIFT offline.
- **LAN Companion Điện thoại (`:8766`) & Phím tắt thông minh:** Quét QR Wi-Fi cục bộ xem bản đồ 1:1, tuyến đường, ảnh hướng dẫn & đánh dấu ghim ngay trên điện thoại; phím mặc định `Alt` tay trái không dùng `Shift/Ctrl` + đổi phím NVIDIA Overlay có backup 1 chạm.
- **Tự cập nhật xác thực SHA-256 & Nâng quyền không hiện UAC:** Kiểm tra bản mới kèm xác thực `SHA256SUMS.txt` (FIPS 180-4) và tác vụ nâng quyền `GenshinHelper_Elevated`.

| Mini-HUD, Thẻ Hướng Dẫn & Live Radar theo Tuyến | Bằng chứng 100% CV Thụ động (~2% CPU · 181 MB RAM) |
| :---: | :---: |
| <img src="docs/media/v57-radar-guide-vi.jpg" alt="Mini-HUD, Thẻ Hướng Dẫn và Bản đồ nổi Live Radar vẽ tuyến đường ngay trong game" width="100%" /><br/><sub>Mini-HUD 360°, Thẻ Hướng Dẫn (<code>Alt + H</code>) &amp; đường nối tuyến trên Live Radar</sub> | <img src="docs/media/v57-cv-proof-vi.jpg" alt="Task Manager chứng minh chỉ dùng ~2% CPU, 181 MB RAM và chạy được ngay trên video phát lại" width="100%" /><br/><sub>Theo dõi mượt ngay cả trên video phát lại mà không cần mở game hay đọc RAM</sub> |

<details>
<summary><strong>📸 Xem thêm ảnh minh họa: Phủ tuyến Map (M), LAN Companion Điện thoại &amp; Extension Popup</strong></summary>
<br/>

| Phủ Tuyến đường trực tiếp lên Map (`M`) | LAN Companion trên Điện thoại (Tuyến & Hướng dẫn) |
| :---: | :---: |
| <img src="docs/media/v57-mmap-route-vi.jpg" alt="Tuyến đường HoYoLAB phủ trực tiếp lên bản đồ toàn màn hình M trong game" width="100%" /><br/><sub>Hiển thị từng chặng tuyến đường HoYoLAB &amp; điểm dịch chuyển gần nhất ngay trên Map (<code>M</code>)</sub> | <img src="docs/media/v57-lan-mobile-vi.jpg" alt="LAN Companion trên điện thoại đồng bộ tuyến đường và xem ảnh hướng dẫn ghim" width="100%" /><br/><sub>Quét QR Wi-Fi cục bộ mở bản đồ 1:1 trên điện thoại, bấm ghim xem hướng dẫn &amp; đánh dấu nhanh</sub> |

<p align="center">
  <img src="docs/media/extension-popup.jpg" alt="Bản đồ HoYoLAB gọn gàng cùng bảng điều khiển Extension Popup" width="88%" />
  <br/>
  <sub><em>Bảng điều khiển Extension Control Center (MV3) trên nền HoYoLAB Interactive Map gọn sạch</em></sub>
</p>
</details>

---

## Cài đặt nhanh

1. Tải **`GenshinHelper-Setup-v5.8.0.exe`** (`~8.5 MB`, `0` DLL ngoài) tại [**GitHub Releases**](https://github.com/tuanminh26596/Genshin-helper/releases/latest) và chạy với quyền **Administrator** (bắt buộc do Windows UIPI để phím tắt/HUD hoạt động đè lên game).
2. Khi cài xong, làm theo trang **Hướng dẫn Extension** (`install_extension.html`) tự mở: vào `chrome://extensions` (hoặc Edge / Brave) → bật **Developer mode** → **Load unpacked** → dán đường dẫn thư mục `extension` và ghim biểu tượng lên thanh công cụ. Trên Firefox, tải `extension/manifest.json` làm temporary add-on trong `about:debugging#/runtime/this-firefox`.
3. Ở lần chạy đầu, `GenshinHelper.exe` tự tải dữ liệu bản đồ công khai HoYoLAB (~15 MB) về `%LOCALAPPDATA%\GenshinHelper\data` (mất ~10–20 giây một lần duy nhất).
4. Mở [**HoYoLAB Interactive Map**](https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2), đặt Genshin Impact ở **Borderless Windowed**, bấm **`M`** một lần để khóa vị trí và bắt đầu thám hiểm!

<details>
<summary><strong>🛠️ Dành cho Dev: Build từ mã nguồn Rust (1.92+ / Edition 2024), Cờ CLI &amp; Cấu hình</strong></summary>
<br/>

Yêu cầu: [Rustup (`msvc` stable)](https://rustup.rs/) và Visual Studio Build Tools (C++ workload).

```powershell
git clone https://github.com/tuanminh26596/Genshin-helper.git
cd Genshin-helper

Copy-Item config.example.json config.json
cargo build --manifest-path rust/Cargo.toml --release -p gh-app -p gh-host
# (Tùy chọn) Tải trước dữ liệu bản đồ (mặc định cũng tự tải ở lần chạy đầu):
cargo run --manifest-path rust/Cargo.toml --release -p gh-app -- --sync-data
# Đăng ký Native Messaging Host cho thư mục extension/ dev:
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/register_dev_native_host.ps1
```

Chạy thẳng `GenshinHelper.exe` (tự xin quyền Admin; truyền `--no-elevate` hoặc `GH_NO_ELEVATE=1` nếu không muốn; dùng `installer/build_release.ps1` cùng Inno Setup 6 để đóng gói `dist/installer/GenshinHelper-Setup-v5.8.0.exe`).

- **Cờ CLI:** `--sync-data`, `--no-tray`, `--standby`, `--minimized`, `--record-minimap <GIÂY>`, `--port <PORT>`, `--lan-port <LAN_PORT>`, `--config <PATH>`, `--no-mutex`, `--no-elevate`.
- **`hoyolab_sync` (`config.json`):** Tự kiểm tra `map_version` khi khởi động (`"auto_check_on_startup": true`, `"auto_update_mode": "auto_download"` | `"notify_only"` | `"off"`).

</details>

---

## Bảng phím tắt mặc định (`v5.8.0`)

Tổ hợp `Alt` tay trái không dùng `Shift/Ctrl` — tùy chỉnh trực tiếp trong **Extension Popup → Phím tắt** (tự kiểm tra trùng phím):

| Chức năng | Mặc định | Công dụng |
| --- | --- | --- |
| **Ghim / Hoàn tác mục tiêu HUD** | `Alt + F` / `Alt + T` | Đánh dấu mục tiêu trên cả App & Web Map (>25 m nhắc bấm xác nhận) / hoàn tác ghim nhầm. |
| **Thẻ Hướng Dẫn / Mục tiêu kế** | `Alt + H` / `Alt + E` | Bật/tắt bảng ảnh & ghi chú hướng dẫn từ HoYoLAB / chuyển sang ghim gần tiếp theo. |
| **Live Radar / Mini-HUD** | ``Alt + ` `` / `Alt + X` | Bật/tắt Bản đồ nổi Live Radar 60 fps (mặc định TẮT) / ẩn hoặc hiện Mini-HUD. |
| **Chỉnh bố cục / Hủy khóa** | `Alt + F7` / `Alt + F6` | Kéo thả vị trí & đổi cỡ Mini-HUD + Radar / hủy ghim đang khóa dẫn đường. |
| **Sync Map (`M`) / Đảo cửa sổ / Debug** | `Alt + F11` / `Alt + Q` / `Alt + F12` | Chụp Map (`M`) & ghim gần nhất (<60 m) / đảo Game ↔ Web Map / gói chẩn đoán đã che UID. |

> **Mẹo quay video:** Bật **Hiện HUD khi chụp ảnh / quay phim** trong Tab Điều khiển cho OBS / `Win+Shift+S` / Share Screen. Với NVIDIA `Alt + F9`, bật thêm **Desktop Capture** trong NVIDIA Overlay (`Alt + Z`) → Cài đặt → Quyền riêng tư.

---

## An toàn Chống gian lận, Quyền riêng tư & Minh bạch

- **Thụ động 100% — Nói thẳng về rủi ro:** Chính tác giả dùng hằng ngày trên tài khoản cá nhân với nguyên tắc **0 đọc/ghi RAM**, **0 inject DLL**, **0 hook DirectX** và **0 `SendInput`**. Dù vậy, đây vẫn là **phần mềm bên thứ ba không chính thức** không liên kết với HoYoverse — không ai cam kết 0% rủi ro; **bạn tự cân nhắc và chịu trách nhiệm khi sử dụng.**
- **Không thu thập tài khoản / Không telemetry:** Không bao giờ đọc cookie hay mật khẩu HoYoLAB; chỉ mở server cục bộ `127.0.0.1:8765`; gói chẩn đoán (`Alt + F12`) tự động làm mờ góc UID.
- **Mã nguồn mở & SmartScreen:** File `.exe` mã nguồn mở chưa ký chữ ký thương mại có dùng phím tắt toàn cục (`WH_KEYBOARD_LL`) có thể gặp cảnh báo SmartScreen. Mọi bản phát hành đều kèm `SHA256SUMS.txt` và có thể tự kiểm tra / build từ nguồn.

<details>
<summary><strong>📋 Bảng tương thích chi tiết, Miễn trừ đầy đủ &amp; Kiến trúc hệ thống</strong></summary>
<br/>

| Cài đặt / Môi trường | Mức hỗ trợ | Hành vi & Khuyến nghị |
| --- | --- | --- |
| **Borderless Windowed / Windowed** | Hỗ trợ đầy đủ | Khuyên dùng cho Mini-HUD, Live Radar, Tuyến đường Map (`M`) & chơi 1 màn hình. |
| **Exclusive Fullscreen** *(Giới hạn OS)* | Không hiện overlay | Windows DWM chặn lớp phủ. Dùng nút **Mở Borderless 1-click** tích hợp (`-popupwindow -screen-fullscreen 0`). |
| **Quyền Admin / UIPI** *(Giới hạn Windows)* | Cần quyền Admin | Bộ cài tạo tác vụ `GenshinHelper_Elevated` bật không hiện lại UAC (tự dọn sạch khi gỡ cài đặt). |
| **Windows HDR / Auto-HDR** | Hỗ trợ đầy đủ *(Tự động WGC)* | Tự nhận diện HDR qua DXGI (`IDXGIOutput6`), chụp WGC 16-bit float kèm tone-map, tự lùi về GDI `SRCCOPY` trên SDR. |
| **Tỷ lệ màn / DPI / Tay cầm / Xoay Minimap** | Hỗ trợ đầy đủ | Hỗ trợ `16:9`, `16:10`, `21:9`, `4:3`, DPI `100%–200%`, Phím/Chuột & Tay cầm, khóa hướng Bắc & xoay theo góc nhìn. |

- **Đổi phím NVIDIA Overlay:** Chỉ thay đổi khi bạn bấm xác nhận trong Popup, luôn sao lưu tự động để hoàn tác 1 chạm.
- **Phạm vi & Giấy phép:** Chỉ hỗ trợ Genshin Impact PC trên Windows 10/11 64-bit; phát hành theo [Giấy phép MIT](LICENSE) nguyên trạng (**AS IS**).

```text
+----------------------------------+          localhost:8765 (HTTP)         +---------------------------------+
|  Genshin-Helper Thuần Rust (x64) | <====================================> | Chrome / Edge / Brave / Firefox Extension |
|  GenshinHelper.exe (0 DLL ngoài) |  Ring buffer /poll?since=<seq> lossless|  (MV3 background + Popup)       |
|  - Chụp WGC HDR / GDI SRCCOPY    |                                        |  - Cầu nối HoYoLAB Map & Route  |
|  - Định vị Map M & Minimap Rust  |        Native Messaging (stdio)        |  - Đồng bộ nền không giảm nhịp  |
|  - Mini-HUD, Live Radar & Map M  | <------------------------------------> |  - Tuyến TSP & Xuất ghim mở     |
|  - Cache cục bộ (%LOCALAPPDATA%) |   GenshinHelperHost.exe (ID cố định)   +---------------------------------+
+----------------------------------+
```

- **Cộng đồng (Discord):** [Tham gia Server Discord](https://discord.gg/DdA4qyvqH3)
- **Tài liệu:** [extension/README.md](extension/README.md) · [CONTRIBUTING.md](CONTRIBUTING.md) · [SECURITY.md](SECURITY.md) · [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)

</details>

---

## Ủng hộ tác giả (Donate)

Genshin-Helper **miễn phí 100%, mở mã nguồn và không quảng cáo** — mọi tính năng đều mở trọn vẹn. Mỗi lần ra vùng đất mới (như Nod-Krai hay Snezhnaya sắp tới) đều cần thời gian cập nhật bản đồ, tầng hang ngầm và kiểm thử CV. Nếu công cụ giúp hành trình khám phá Teyvat của bạn nhàn hơn, một ly cà phê tự nguyện sẽ là động lực rất lớn để mình duy trì cập nhật nhanh & ổn định:

| Ủng hộ Quốc tế | Ủng hộ tại Việt Nam (MoMo / VietQR) |
| :---: | :---: |
| <a href="https://ko-fi.com/minmories"><img src="https://img.shields.io/badge/Support%20on%20Ko--fi-minmories-FF5E5B?style=for-the-badge&logo=kofi&logoColor=white" alt="Ủng hộ qua Ko-fi" /></a><br/><br/><strong>Ko-fi:</strong> <https://ko-fi.com/minmories><br/><strong>GitHub Sponsors:</strong> <code>@tuanminh26596</code> <em>(đang chờ duyệt)</em><br/><strong>Discord:</strong> <https://discord.gg/DdA4qyvqH3><br/><br/><sub>Hoàn toàn tự nguyện — cảm ơn bạn đã đồng hành cùng dự án!</sub> | <img src="assets/donate/momo_qr.jpg" alt="QR MoMo ủng hộ Genshin-Helper" width="180" /><br/><sub>Quét bằng MoMo hoặc app ngân hàng (VietQR)</sub> |
