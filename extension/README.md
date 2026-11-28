# Genshin Helper — Browser Extension Control Center (MV3, v5.8.0)

*Bilingual Guide: **English** below · **Tiếng Việt** further down.*

---

## English

Manifest V3 extension for **Google Chrome**, **Microsoft Edge**, **Brave**, and **Firefox** (128+). Acts as the Control Center and passive HoYoLAB Interactive Map bridge:

- **Zero-Clutter Map Surface:** All controls, toggles, smart hotkey conflict detection, and diagnostics live inside the **Extension Popup**. Only the dual-rotation player arrow, auto-fading mini-toasts, and diagnostic modal render over the map.
- **Zero-Throttle Background Sync:** `/poll?since=<seq>` runs inside the MV3 background context (`background.js`; a Service Worker in Chromium and a non-persistent event page in Firefox), keeping position sync and Mini-HUD updates responsive even when the browser tab is hidden behind the game.
- **Power Badge States:**
  - `ON` (Green): Desktop engine active.
  - `ZZZ` (Amber): Engine paused in Standby (0% CPU); click **ON** in Popup to resume immediately.
  - `OFF` (Red): Desktop App exited; click **ON** in Popup to wake automatically via Native Messaging (`com.genshinhelper.host` — requires Setup Installer or `scripts/register_dev_native_host.ps1` on Windows). Firefox users should start the desktop app manually for now.
- **Off-Store Installation (`extension/`):**
  1. Open `chrome://extensions` (or `edge://extensions` / `brave://extensions`).
  2. Enable **Developer mode**, click **Load unpacked**, and select the `extension/` directory (pinned Extension ID: `nfkijdoblmphmdljfogooajhbldkeojh`).
  3. Pin the **Genshin Helper compass icon** to your toolbar and refresh any open HoYoLAB Interactive Map tabs (`F5`).

### Firefox (128+)

1. Start the desktop app manually; native-messaging wake is not required for map sync.
2. Open `about:debugging#/runtime/this-firefox`, click **Load Temporary Add-on**, and select `extension/manifest.json`.
3. Open the HoYoLAB Interactive Map and pin the extension to the toolbar.

The Firefox add-on ID is `genshin-helper@tuannm26596.github.io` (declared in
`browser_specific_settings.gecko.id`). Firefox uses `background.scripts`; Chromium
uses `background.service_worker`, so the same `extension/` directory works in both.


---

## Tiếng Việt

Tiện ích mở rộng Manifest V3 cho **Google Chrome**, **Microsoft Edge**, **Brave** và **Firefox** (128+), đóng vai trò trung tâm điều khiển (**Control Center**) và cầu nối bản đồ HoYoLAB:

- **Bề mặt bản đồ sạch 100%:** Toàn bộ công tắc, quản lý phím tắt chống xung đột, công cụ TSP/Xuất ghim và chẩn đoán đều nằm gọn trong **Extension Popup**.
- **Chạy ngầm không giảm nhịp:** Vòng lặp `/poll?since=<seq>` chạy trong background MV3 (`background.js`; Service Worker trên Chromium và event page trên Firefox), nên vẫn đồng bộ tức thời ngay cả khi tab HoYoLAB bị cửa sổ game che khuất trên 1 màn hình.
- **Các trạng thái nguồn trên thanh công cụ:**
  - `ON` (Xanh lá): Desktop Engine đang chạy bình thường.
  - `ZZZ` (Vàng hổ phách): Đang tạm nghỉ Standby (0% CPU) — bấm **BẬT** trong Popup để bật lại tức thì.
  - `OFF` (Đỏ): Desktop App đã thoát hẳn — bấm **BẬT** để tự khởi chạy qua Native Messaging (`com.genshinhelper.host`, được cài sẵn bởi bộ Setup trên Windows). Người dùng Firefox hiện cần mở app desktop thủ công.
- **Cài đặt thủ công (Load Unpacked):**
  1. Mở `chrome://extensions` (hoặc `edge://extensions` / `brave://extensions`).
  2. Bật **Developer mode** → nhấn **Load unpacked** → chọn thư mục **`extension/`** (giữ nguyên ID cố định `nfkijdoblmphmdljfogooajhbldkeojh`).
  3. Ghim **biểu tượng la bàn Genshin Helper** lên thanh công cụ và bấm **F5** tải lại các tab HoYoLAB Map đang mở.

### Firefox (128+)

1. Mở app desktop thủ công; không cần native messaging để đồng bộ bản đồ.
2. Mở `about:debugging#/runtime/this-firefox`, chọn **Load Temporary Add-on** và trỏ tới `extension/manifest.json`.
3. Mở HoYoLAB Interactive Map và ghim extension lên thanh công cụ.

Firefox dùng add-on ID `genshin-helper@tuannm26596.github.io` (khai báo trong
`browser_specific_settings.gecko.id`). Firefox dùng `background.scripts`, Chromium
dùng `background.service_worker`, nên cùng thư mục `extension/` chạy được trên cả hai.
