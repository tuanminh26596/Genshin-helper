# Genshin Helper — Browser Extension Control Center (MV3, v5.8.0)

*Bilingual Guide: **English** below · **Tiếng Việt** further down.*

---

## English

Manifest V3 extension for **Google Chrome**, **Microsoft Edge**, and **Brave**. Acts as the Control Center and passive HoYoLAB Interactive Map bridge:

- **Zero-Clutter Map Surface:** All controls, toggles, smart hotkey conflict detection, and diagnostics live inside the **Extension Popup**. Only the dual-rotation player arrow, auto-fading mini-toasts, and diagnostic modal render over the map.
- **Zero-Throttle Background Sync:** `/poll?since=<seq>` runs inside the MV3 background Service Worker (`background.js`), keeping position sync and Mini-HUD updates responsive even when the browser tab is hidden behind the game.
- **Power Badge States:**
  - `ON` (Green): Desktop engine active.
  - `ZZZ` (Amber): Engine paused in Standby (0% CPU); click **ON** in Popup to resume immediately.
  - `OFF` (Red): Desktop App exited; click **ON** in Popup to wake automatically via Native Messaging (`com.genshinhelper.host` — requires Setup Installer or `scripts/register_dev_native_host.ps1`).
- **Off-Store Installation (`extension/`):**
  1. Open `chrome://extensions` (or `edge://extensions` / `brave://extensions`).
  2. Enable **Developer mode**, click **Load unpacked**, and select the `extension/` directory (pinned Extension ID: `nfkijdoblmphmdljfogooajhbldkeojh`).
  3. Pin the **Genshin Helper compass icon** to your toolbar and refresh any open HoYoLAB Interactive Map tabs (`F5`).

---

## Tiếng Việt

Tiện ích mở rộng Manifest V3 cho **Google Chrome**, **Microsoft Edge** và **Brave**, đóng vai trò trung tâm điều khiển (**Control Center**) và cầu nối bản đồ HoYoLAB:

- **Bề mặt bản đồ sạch 100%:** Toàn bộ công tắc, quản lý phím tắt chống xung đột, công cụ TSP/Xuất ghim và chẩn đoán đều nằm gọn trong **Extension Popup**.
- **Chạy ngầm không giảm nhịp:** Vòng lặp `/poll?since=<seq>` chạy trong Service Worker nền (`background.js`), nên vẫn đồng bộ tức thời ngay cả khi tab HoYoLAB bị cửa sổ game che khuất trên 1 màn hình.
- **Các trạng thái nguồn trên thanh công cụ:**
  - `ON` (Xanh lá): Desktop Engine đang chạy bình thường.
  - `ZZZ` (Vàng hổ phách): Đang tạm nghỉ Standby (0% CPU) — bấm **BẬT** trong Popup để bật lại tức thì.
  - `OFF` (Đỏ): Desktop App đã thoát hẳn — bấm **BẬT** để tự khởi chạy qua Native Messaging (`com.genshinhelper.host`, được cài sẵn bởi bộ Setup).
- **Cài đặt thủ công (Load Unpacked):**
  1. Mở `chrome://extensions` (hoặc `edge://extensions` / `brave://extensions`).
  2. Bật **Developer mode** → nhấn **Load unpacked** → chọn thư mục **`extension/`** (giữ nguyên ID cố định `nfkijdoblmphmdljfogooajhbldkeojh`).
  3. Ghim **biểu tượng la bàn Genshin Helper** lên thanh công cụ và bấm **F5** tải lại các tab HoYoLAB Map đang mở.
