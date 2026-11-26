(function (globalScope) {
  'use strict';

  const PINNED_EXT_ID = 'nfkijdoblmphmdljfogooajhbldkeojh';
  // Language source of truth: chrome.storage.local[gh_lang], written ONLY when the user picks
  // VI | EN (popup toggle, or a language change pushed by the desktop app). Without it the
  // desktop app's language applies, then the browser's. localStorage[gh_lang_cache] (popup and
  // HoYoLAB page) is only a start-up cache and never counts as a preference.
  const STORAGE_KEY_LANG = 'gh_lang';
  const STORAGE_KEY_LANG_CACHE = 'gh_lang_cache';
  // Pre-5.5.0-r6 localStorage key: it also held browser-detected defaults marked as explicit
  // (which pinned the HoYoLAB page to English over the app's Vietnamese); dropped on load.
  const LEGACY_LOCAL_KEY_LANG = 'gh_lang';
  const STORAGE_KEY_COMPACT = 'gh_compact_ui';
  const SUPPORTED_LANGS = ['vi', 'en'];
  const DEFAULT_LANG = 'en';

  function normalizeLang(raw) {
    if (!raw || typeof raw !== 'string') return null;
    const cleaned = raw.trim().toLowerCase().replace(/_/g, '-');
    if (!cleaned || cleaned === 'auto' || cleaned === 'system' || cleaned === 'default') {
      return null;
    }
    if (cleaned === 'vi' || cleaned.startsWith('vi-') || cleaned.startsWith('vietnamese')) {
      return 'vi';
    }
    if (cleaned === 'en' || cleaned.startsWith('en-') || cleaned.startsWith('english')) {
      return 'en';
    }
    return null;
  }

  function detectBrowserLang() {
    try {
      if (typeof navigator !== 'undefined') {
        const list = Array.isArray(navigator.languages) && navigator.languages.length > 0
          ? navigator.languages
          : [navigator.language || ''];
        for (const item of list) {
          const norm = normalizeLang(item);
          if (norm) return norm;
        }
      }
    } catch (e) {}
    return DEFAULT_LANG;
  }

  function resolveLanguage(setting) {
    const explicit = normalizeLang(setting);
    if (explicit) return explicit;
    return detectBrowserLang();
  }

  function interpolate(template, params) {
    if (!params || typeof params !== 'object') return template;
    return String(template).replace(/\{(\w+)\}/g, (match, name) => {
      if (Object.prototype.hasOwnProperty.call(params, name) && params[name] !== undefined && params[name] !== null) {
        return String(params[name]);
      }
      return match;
    });
  }

  function createI18n(initialBundles, initialLang) {
    let bundles = {
      vi: (initialBundles && initialBundles.vi) ? { ...initialBundles.vi } : {},
      en: (initialBundles && initialBundles.en) ? { ...initialBundles.en } : {}
    };
    let currentLang = resolveLanguage(initialLang);
    let ready = Boolean(Object.keys(bundles.vi).length > 0 || Object.keys(bundles.en).length > 0);
    const listeners = new Set();

    function notify() {
      for (const fn of Array.from(listeners)) {
        try {
          fn(currentLang);
        } catch (e) {}
      }
    }

    function setBundles(nextBundles) {
      if (!nextBundles || typeof nextBundles !== 'object') return;
      if (nextBundles.vi && typeof nextBundles.vi === 'object') {
        bundles.vi = { ...nextBundles.vi };
      }
      if (nextBundles.en && typeof nextBundles.en === 'object') {
        bundles.en = { ...nextBundles.en };
      }
      ready = Boolean(Object.keys(bundles.vi).length > 0 || Object.keys(bundles.en).length > 0);
      notify();
    }

    function t(key, params, overrideLang) {
      if (typeof key !== 'string') return String(key);
      const lang = overrideLang ? resolveLanguage(overrideLang) : currentLang;
      const primary = bundles[lang] || {};
      const fallback = bundles[DEFAULT_LANG] || {};

      let val = primary[key];
      if (typeof val !== 'string' || val.trim() === '') {
        val = fallback[key];
      }
      if (typeof val !== 'string' || val.trim() === '') {
        val = key;
      }
      return interpolate(val, params);
    }

    function getLang() {
      return currentLang;
    }

    function setLangSync(nextLang) {
      const resolved = resolveLanguage(nextLang);
      const changed = resolved !== currentLang;
      currentLang = resolved;
      if (changed) notify();
      return currentLang;
    }

    function applyDom(root) {
      if (!ready) return;
      const target = root || (typeof document !== 'undefined' ? document : null);
      if (!target || typeof target.querySelectorAll !== 'function') return;

      const textEls = target.querySelectorAll('[data-i18n]');
      for (const el of textEls) {
        const key = el.getAttribute('data-i18n');
        if (key) {
          el.textContent = t(key);
        }
      }
      const titleEls = target.querySelectorAll('[data-i18n-title]');
      for (const el of titleEls) {
        const key = el.getAttribute('data-i18n-title');
        if (key) {
          el.setAttribute('title', t(key));
        }
      }
      const placeholderEls = target.querySelectorAll('[data-i18n-placeholder]');
      for (const el of placeholderEls) {
        const key = el.getAttribute('data-i18n-placeholder');
        if (key) {
          el.setAttribute('placeholder', t(key));
        }
      }
      const ariaEls = target.querySelectorAll('[data-i18n-aria-label]');
      for (const el of ariaEls) {
        const key = el.getAttribute('data-i18n-aria-label');
        if (key) {
          el.setAttribute('aria-label', t(key));
        }
      }
    }

    function onChange(fn) {
      if (typeof fn !== 'function') return () => {};
      listeners.add(fn);
      return () => listeners.delete(fn);
    }

    return {
      resolveLanguage,
      normalizeLang,
      setBundles,
      t,
      getLang,
      setLangSync,
      applyDom,
      onChange,
      isReady: () => ready
    };
  }

  const EMBEDDED_PAGE_BUNDLES = {
    "vi": {
      "page.area.fallback": "Vùng {id}",
      "page.area.switched": "🗺️ Tự động chuyển khu vực sang: [{name}]!",
      "page.autosync.off": "⏸️ Tự đồng bộ Map (M): TẮT",
      "page.autosync.on": "⚡ Tự đồng bộ Map (M): BẬT",
      "page.compact.disabled": "✨ Giao diện HoYoLAB gọn: TẮT",
      "page.compact.enabled": "✨ Giao diện HoYoLAB gọn: BẬT",
      "page.diag.captured_ok": "📸 Đã chụp chẩn đoán: [{folder}]! Hãy nhập ghi chú lỗi vào hộp thoại.",
      "page.diag.captured_warn": "📸 Đã chụp chẩn đoán: [{folder}]! {reason}",
      "page.diag.modal.badge_ok": "Đã định vị tọa độ",
      "page.diag.modal.badge_warn": "Chưa đủ mốc / Ngoại màn hình",
      "page.diag.modal.bundle_prefix": "Bundle: {folder}",
      "page.diag.modal.close": "Đóng",
      "page.diag.modal.copied_discord": "Đã chép mẫu Discord — kéo file .zip và dán vào kênh #support-and-bugs.",
      "page.diag.modal.copy_discord": "Chép mẫu Discord",
      "page.diag.modal.discord_footer": "Đính kèm file {zip} (đã ẩn UID & thông tin cá nhân)",
      "page.diag.modal.discord_hint": "Ảnh & log đã lưu sẵn vào thư mục (chưa nén .zip để tránh thừa). Bấm \"Lưu ghi chú\" để ghi nhanh, hoặc \"Lưu & Mở .zip\" khi muốn xuất gói gửi Discord.",
      "page.diag.modal.folder_error": "Không thể mở thư mục (kiểm tra Desktop App đang chạy).",
      "page.diag.modal.folder_opened": "Đã mở thư mục trên Windows Explorer.",
      "page.diag.modal.offline_error": "Không thể kết nối server desktop (127.0.0.1:8765)!",
      "page.diag.modal.open_folder": "Mở thư mục",
      "page.diag.modal.placeholder": "Nhập ghi chú lỗi cụ thể tại đây (vd: kéo map sang Fontaine nhưng browser vẫn nhảy theo)...",
      "page.diag.modal.save": "Lưu & Mở .zip",
      "page.diag.modal.save_error": "Không lưu được ghi chú.",
      "page.diag.modal.save_only": "Lưu ghi chú",
      "page.diag.modal.saved_inline": "Đã nén {zip}, mở thư mục chứa file & chép sẵn mẫu Discord!",
      "page.diag.modal.saved_only_inline": "Đã lưu ghi chú vào {folder}/05_user_note.txt (không tạo .zip).",
      "page.diag.modal.saved_toast": "📝 Đã lưu ghi chú lỗi vào [{folder}]/05_user_note.txt",
      "page.diag.modal.saving": "Đang lưu...",
      "page.diag.modal.shortcut_hint": "Ctrl + Enter: lưu ghi chú nhanh • Esc: đóng (ảnh & log đã lưu)",
      "page.diag.modal.skip": "Đóng (đã lưu)",
      "page.diag.modal.title": "📸 Chụp Chẩn Đoán Thành Công",
      "page.diag.tag.no_cave_img": "[Web không hiện ảnh hang]",
      "page.diag.tag.other": "[Khác]",
      "page.diag.tag.panning_no_arrow": "[Đang kéo map / Không có mũi tên]",
      "page.diag.tag.wrong_floor": "[Sai phân tầng]",
      "page.diag.tag.wrong_pos": "[Sai vị trí]",
      "page.engine.resumed": "⚡ Server đã hoạt động trở lại!",
      "page.engine.standby": "💤 Server đang tạm nghỉ (Standby — 0% CPU)",
      "page.export.done": "✅ Đã xuất [{format}]: {matched} khớp • {unmatched} chưa khớp • {missing} thiếu metadata",
      "page.export.error": "❌ Lỗi xuất ghim: {error}",
      "page.export.fetching": "📦 Đang tải đầy đủ danh sách điểm & ghim HoYoLAB ({format})...",
      "page.export.login_required": "⚠️ Bạn cần đăng nhập tài khoản HoYoLAB trên trang Map để xuất ghim.",
      "page.hotkey.updated": "⚙️ Phím tắt [{action}] đã đổi thành: [{name}]",
      "page.layer.activated_floor": "✅ Đã kích hoạt phân tầng: [{label}]!",
      "page.layer.activating": "🗺️ Đang kích hoạt [Thám Hiểm Phân Tầng]...",
      "page.layer.default_underground": "Tầng ngầm",
      "page.layer.enter_explore": "🗺️ Tự động mở thám hiểm phân tầng: [{label}]!",
      "page.layer.exit_surface": "🌤️ Đã tự động thoát phân tầng về mặt đất!",
      "page.layer.floor_fallback": "Tầng {idx}",
      "page.layer.layered_label": "Phân tầng",
      "page.layer.selected_floor": "✅ Đã chọn tầng phân tầng: [{label}]!",
      "page.layer.surface_label": "Mặt đất",
      "page.layer.switch_floor": "🗺️ Chuyển tầng phân tầng: [{label}]!",
      "page.marker.title": "Người chơi ({lat}, {lng}){floor}",
      "page.nav.lock_pill_cancel": "Hủy",
      "page.nav.marked": "Đã ghim từ trong game: {name}",
      "page.nav.navigate_btn": "Dẫn đường",
      "page.nav.navigate_err": "Không thể khóa mục tiêu — kiểm tra Desktop App đang chạy.",
      "page.nav.navigate_locked": "🧭 Đang dẫn đường tới: {name}",
      "page.nav.navigate_title": "Khóa mục tiêu ghim này trên Mini-HUD & quay lại game",
      "page.nav.pin_not_found": "Không tìm thấy ghim \"{name}\" trên bản đồ web — hãy bật bộ lọc loại ghim này.",
      "page.nav.unlock_btn": "Hủy dẫn đường",
      "page.nav.unlock_title": "Hủy khóa dẫn đường cho ghim này",
      "page.nav.unlocked": "Đã hủy khóa dẫn đường.",
      "page.nav.unmarked": "Đã bỏ ghim từ trong game: {name}",
      "page.route.captured": "🧭 Đã đồng bộ tuyến Web: {title} ({sections} chặng)",
      "page.route.captured_hint": "🧭 Đã đồng bộ: {title} ({sections} chặng) — Vào game bấm M để xem hướng dẫn",
      "page.route.cleared": "🧹 Đã dừng dẫn đường tuyến.",
      "page.route.export_btn": "Xuất",
      "page.route.focus_btn": "Xem tuyến",
      "page.route.next_sec_title": "Chặng tiếp",
      "page.route.prev_sec_title": "Chặng trước",
      "page.route.reattach_btn": "Về nhân vật",
      "page.route.reattached": "📍 Đã bám lại vị trí nhân vật",
      "page.route.reclick_hint": "🧭 Phát hiện bảng tuyến đang mở — Hãy bấm chọn lại 1 tuyến hoặc tải lại trang để đồng bộ",
      "page.route.reset_btn": "Đặt lại",
      "page.route.reset_done": "↺ Đã đặt lại tiến độ tuyến về đầu chặng 1!",
      "page.route.reset_title": "Đi lại tuyến từ đầu (không đụng ghim đã đánh dấu trên HoYoLAB)",
      "page.route.stop_btn": "Dừng",
      "page.sync.already_marked_title": "Đã ghim từ trước",
      "page.sync.already_marked_toast": "ℹ️ Điểm này ĐÃ ĐƯỢC GHIM từ trước!",
      "page.sync.default_pin_title": "Điểm đánh dấu",
      "page.sync.error_title": "Lỗi đồng bộ",
      "page.sync.error_toast": "❌ Lỗi: {error}",
      "page.sync.low_landmarks": "⚠️ Không đủ mốc định vị thế giới! (Hãy thu nhỏ hoặc kéo bản đồ thấy rõ mốc dịch chuyển)",
      "page.sync.low_landmarks_detail": "Thu nhỏ bản đồ để thấy mốc dịch chuyển",
      "page.sync.low_landmarks_title": "Thiếu mốc định vị",
      "page.sync.map_not_found": "❌ Chưa tìm thấy Leaflet Map! Hãy chắc chắn map đã tải xong.",
      "page.sync.map_not_ready_detail": "Hãy đợi trang map tải xong rồi thử lại",
      "page.sync.map_not_ready_title": "Bản đồ HoYoLAB chưa sẵn sàng",
      "page.sync.map_opened": "✅ Đã mở bản đồ [{name}]! Đang trượt tới vị trí...",
      "page.sync.marked_dist": "Cách {dist}m",
      "page.sync.marked_title": "Đã ghim: {title}",
      "page.sync.marked_toast": "✅ Đã kéo map & GHIM: [{title}]!",
      "page.sync.nearest_opening": "Điểm gần nhất cách người chơi {dist}m. Đang mở...",
      "page.sync.no_mark_btn": "⚠️ Không thấy nút Ghim trên điểm gần nhất.",
      "page.sync.no_markers": "❌ Chưa thấy điểm nào! Hãy chọn bật loại Rương hoặc Thần Đồng ở menu!",
      "page.sync.no_match": "❌ Không tìm thấy điểm phù hợp.",
      "page.sync.no_nearby_title": "Không có điểm gần (< 60m)",
      "page.sync.no_nearby_toast": "✅ Đã căn vị trí & tầng thám hiểm! (Không có điểm gần < 60m)",
      "page.sync.not_detected": "⚠️ Chưa tìm thấy vị trí game! Hãy mở Map (M) trong game rồi kích hoạt lại.",
      "page.sync.not_detected_detail": "Hãy mở Map (M) trong game rồi thử lại",
      "page.sync.not_detected_title": "Không thấy vị trí game",
      "page.sync.panning_to": "Đang trượt bản đồ web tới vị trí game{map}: [{lat}, {lng}]...",
      "page.sync.switching_map": "🔄 Tự động chuyển sang bản đồ [{name}]...",
      "page.sync.synced_pos": "✅ Đã đồng bộ vị trí{floor}!",
      "page.tsp.cleared": "🧹 Đã xóa tuyến đường TSP khỏi bản đồ.",
      "page.tsp.error": "❌ Lỗi lập tuyến TSP: {error}",
      "page.tsp.no_pins": "⚠️ Không có điểm chưa ghim nào hiển thị để lập tuyến TSP.",
      "page.tsp.planned": "🧭 Đã tạo tuyến TSP: {count} điểm ({teleports} lượt dịch chuyển)!",
      "page.tsp.planning": "🧭 Đang tối ưu tuyến đường farm (TSP)...",
      "page.unpin.recenter_btn": "Bám lại vị trí",
      "page.unpin.recenter_title": "Bật lại tự động bám vị trí người chơi",
      "page.unpin.resumed_toast": "🎯 Đã bật lại tự động bám vị trí người chơi",
      "page.unpin.toast": "✋ Đã tạm ngưng bám vị trí — Nhấn nút để căn giữa lại"
    },
    "en": {
      "page.area.fallback": "Area {id}",
      "page.area.switched": "🗺️ Auto-switched area to: [{name}]!",
      "page.autosync.off": "⏸️ Auto-Sync Map (M): OFF",
      "page.autosync.on": "⚡ Auto-Sync Map (M): ON",
      "page.compact.disabled": "✨ Compact HoYoLAB UI: OFF",
      "page.compact.enabled": "✨ Compact HoYoLAB UI: ON",
      "page.diag.captured_ok": "📸 Diagnostic captured: [{folder}]! Enter a bug note in the dialog.",
      "page.diag.captured_warn": "📸 Diagnostic captured: [{folder}]! {reason}",
      "page.diag.modal.badge_ok": "World Coords Locked",
      "page.diag.modal.badge_warn": "Low Landmarks / Off-Screen",
      "page.diag.modal.bundle_prefix": "Bundle: {folder}",
      "page.diag.modal.close": "Close",
      "page.diag.modal.copied_discord": "Discord template copied — drag the .zip and paste in #support-and-bugs.",
      "page.diag.modal.copy_discord": "Copy Discord Form",
      "page.diag.modal.discord_footer": "Attach {zip} (UID & personal data masked)",
      "page.diag.modal.discord_hint": "Screenshots & logs are saved to the folder (no .zip yet). Click \"Save Note\" for a quick note, or \"Save & Open .zip\" when ready to share on Discord.",
      "page.diag.modal.folder_error": "Could not open folder (check if Desktop App is running).",
      "page.diag.modal.folder_opened": "Opened Diagnostics folder in Explorer.",
      "page.diag.modal.offline_error": "Cannot connect to desktop server (127.0.0.1:8765)!",
      "page.diag.modal.open_folder": "Open Folder",
      "page.diag.modal.placeholder": "Describe the issue here (e.g., dragged map to Fontaine but browser snapped back)...",
      "page.diag.modal.save": "Save & Open .zip",
      "page.diag.modal.save_error": "Could not save note.",
      "page.diag.modal.save_only": "Save Note",
      "page.diag.modal.saved_inline": "Packaged {zip}, opened folder & copied Discord template!",
      "page.diag.modal.saved_only_inline": "Saved note to {folder}/05_user_note.txt (no .zip created).",
      "page.diag.modal.saved_toast": "📝 Saved diagnostic note to [{folder}]/05_user_note.txt",
      "page.diag.modal.saving": "Saving...",
      "page.diag.modal.shortcut_hint": "Ctrl + Enter: save note only • Esc: close (capture saved)",
      "page.diag.modal.skip": "Close (saved)",
      "page.diag.modal.title": "📸 Diagnostic Capture Saved",
      "page.diag.tag.no_cave_img": "[Cave Layer Missing]",
      "page.diag.tag.other": "[Other]",
      "page.diag.tag.panning_no_arrow": "[Dragging Map / No Arrow]",
      "page.diag.tag.wrong_floor": "[Wrong Floor]",
      "page.diag.tag.wrong_pos": "[Wrong Position]",
      "page.engine.resumed": "⚡ Server engine resumed!",
      "page.engine.standby": "💤 Server engine paused (Standby — 0% CPU)",
      "page.export.done": "✅ Exported [{format}]: {matched} matched • {unmatched} unmatched • {missing} missing metadata",
      "page.export.error": "❌ Export failed: {error}",
      "page.export.fetching": "📦 Fetching full HoYoLAB point list & marks ({format})...",
      "page.export.login_required": "⚠️ Please sign in to HoYoLAB on the Map page to export marked pins.",
      "page.hotkey.updated": "⚙️ Hotkey [{action}] updated to: [{name}]",
      "page.layer.activated_floor": "✅ Activated layered floor: [{label}]!",
      "page.layer.activating": "🗺️ Activating [Layered Exploration]...",
      "page.layer.default_underground": "Underground",
      "page.layer.enter_explore": "🗺️ Entered layered exploration: [{label}]!",
      "page.layer.exit_surface": "🌤️ Returned to surface layer!",
      "page.layer.floor_fallback": "Floor {idx}",
      "page.layer.layered_label": "Layered",
      "page.layer.selected_floor": "✅ Selected layered floor: [{label}]!",
      "page.layer.surface_label": "Surface",
      "page.layer.switch_floor": "🗺️ Switched underground floor: [{label}]!",
      "page.marker.title": "Player ({lat}, {lng}){floor}",
      "page.nav.lock_pill_cancel": "Cancel",
      "page.nav.marked": "Marked from in-game: {name}",
      "page.nav.navigate_btn": "Navigate",
      "page.nav.navigate_err": "Could not lock target — check if Desktop App is running.",
      "page.nav.navigate_locked": "🧭 Navigating to: {name}",
      "page.nav.navigate_title": "Lock this pin on the in-game Mini-HUD & switch back to game",
      "page.nav.pin_not_found": "Pin \"{name}\" not found on the web map — enable this pin category filter.",
      "page.nav.unlock_btn": "Cancel Nav",
      "page.nav.unlock_title": "Clear the locked navigation target",
      "page.nav.unlocked": "Navigation target lock cleared.",
      "page.nav.unmarked": "Unmarked from in-game: {name}",
      "page.route.captured": "🧭 Synced Web Route: {title} ({sections} sections)",
      "page.route.captured_hint": "🧭 Synced route: {title} ({sections} sections) — Press M in-game for overlay",
      "page.route.cleared": "🧹 Web route stopped.",
      "page.route.export_btn": "Export",
      "page.route.focus_btn": "View Route",
      "page.route.next_sec_title": "Next section",
      "page.route.prev_sec_title": "Previous section",
      "page.route.reattach_btn": "To Player",
      "page.route.reattached": "📍 Re-attached camera to player",
      "page.route.reclick_hint": "🧭 Route drawer already open — Click a route once or reload page to sync",
      "page.route.reset_btn": "Reset",
      "page.route.reset_done": "↺ Route progress reset to section 1!",
      "page.route.reset_title": "Restart route from section 1 (does not change HoYoLAB marks)",
      "page.route.stop_btn": "Stop",
      "page.sync.already_marked_title": "Already Marked",
      "page.sync.already_marked_toast": "ℹ️ This pin is ALREADY MARKED!",
      "page.sync.default_pin_title": "Map Pin",
      "page.sync.error_title": "Sync Error",
      "page.sync.error_toast": "❌ Error: {error}",
      "page.sync.low_landmarks": "⚠️ Not enough world landmarks! (Zoom out or pan until teleport waypoints are visible)",
      "page.sync.low_landmarks_detail": "Zoom out to reveal teleport waypoints",
      "page.sync.low_landmarks_title": "Missing landmarks",
      "page.sync.map_not_found": "❌ Leaflet Map not found! Make sure the map has finished loading.",
      "page.sync.map_not_ready_detail": "Wait for the map page to load and try again",
      "page.sync.map_not_ready_title": "HoYoLAB Map not ready",
      "page.sync.map_opened": "✅ Opened map [{name}]! Panning to position...",
      "page.sync.marked_dist": "{dist}m away",
      "page.sync.marked_title": "Marked: {title}",
      "page.sync.marked_toast": "✅ Synced & MARKED: [{title}]!",
      "page.sync.nearest_opening": "Nearest pin is {dist}m away. Opening...",
      "page.sync.no_mark_btn": "⚠️ Mark button not found on nearest pin.",
      "page.sync.no_markers": "❌ No pins visible! Enable Chests or Oculi in the filter sidebar.",
      "page.sync.no_match": "❌ No matching pin found.",
      "page.sync.no_nearby_title": "No nearby pin (< 60m)",
      "page.sync.no_nearby_toast": "✅ Aligned position & floor! (No nearby pin < 60m)",
      "page.sync.not_detected": "⚠️ Game position not found! Open the in-game Map (M) and retry.",
      "page.sync.not_detected_detail": "Open Map (M) in-game and retry",
      "page.sync.not_detected_title": "Game position not found",
      "page.sync.panning_to": "Panning web map to game position{map}: [{lat}, {lng}]...",
      "page.sync.switching_map": "🔄 Auto-switching to map [{name}]...",
      "page.sync.synced_pos": "✅ Synced position{floor}!",
      "page.tsp.cleared": "🧹 Cleared TSP route from map.",
      "page.tsp.error": "❌ TSP route error: {error}",
      "page.tsp.no_pins": "⚠️ No visible uncollected pins available to plan a TSP route.",
      "page.tsp.planned": "🧭 TSP route planned: {count} pins ({teleports} teleports)!",
      "page.tsp.planning": "🧭 Optimizing farming route (TSP)...",
      "page.unpin.recenter_btn": "Re-center",
      "page.unpin.recenter_title": "Resume auto-following player position",
      "page.unpin.resumed_toast": "🎯 Resumed auto-following player position",
      "page.unpin.toast": "✋ Auto-follow paused — Click button to re-center"
    }
  };

  function mergeWithEmbeddedPageBundles(bundles) {
    const viIn = (bundles && bundles.vi && typeof bundles.vi === 'object') ? bundles.vi : {};
    const enIn = (bundles && bundles.en && typeof bundles.en === 'object') ? bundles.en : {};
    if (Object.keys(viIn).length === 0 && Object.keys(enIn).length === 0) {
      return null;
    }
    return {
      vi: { ...EMBEDDED_PAGE_BUNDLES.vi, ...viIn },
      en: { ...EMBEDDED_PAGE_BUNDLES.en, ...enIn }
    };
  }

  // Export cleanly for Node / offline verification
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = {
      SUPPORTED_LANGS,
      DEFAULT_LANG,
      EMBEDDED_PAGE_BUNDLES,
      resolveLanguage,
      normalizeLang,
      createI18n
    };
  }

  if (typeof window === 'undefined') {
    return;
  }

  const hasChromeRuntime = Boolean(
    typeof chrome !== 'undefined' &&
    chrome.runtime &&
    chrome.runtime.id &&
    typeof chrome.runtime.getURL === 'function'
  );
  const isExtensionPage = window.location.protocol === 'chrome-extension:';
  const hostLower = String(window.location.hostname || '').toLowerCase();
  const isHoyolabMap = (hostLower.includes('hoyolab.com') &&
    window.location.pathname.includes('/interactive-map/')) ||
    hostLower === 'v3.yuanshen.site' ||
    hostLower === 'yuanshen.site' ||
    hostLower === 'genshin-impact-map.appsample.com';

  async function fetchJsonSafe(url) {
    try {
      const res = await fetch(url);
      if (!res.ok) return {};
      const parsed = await res.json();
      return (parsed && typeof parsed === 'object') ? parsed : {};
    } catch (e) {
      return {};
    }
  }

  async function fetchLocaleBundles(baseUrlResolver) {
    const [vi, en] = await Promise.all([
      fetchJsonSafe(baseUrlResolver('locales/vi.json')),
      fetchJsonSafe(baseUrlResolver('locales/en.json'))
    ]);
    return { vi, en };
  }

  function storageGet(keys) {
    return new Promise((resolve) => {
      try {
        if (typeof chrome !== 'undefined' && chrome.storage && chrome.storage.local) {
          chrome.storage.local.get(keys, (items) => {
            void (chrome.runtime && chrome.runtime.lastError);
            resolve(items || {});
          });
          return;
        }
      } catch (e) {}
      resolve({});
    });
  }

  function storageSet(obj) {
    return new Promise((resolve) => {
      try {
        if (typeof chrome !== 'undefined' && chrome.storage && chrome.storage.local) {
          chrome.storage.local.set(obj, () => {
            void (chrome.runtime && chrome.runtime.lastError);
            resolve(true);
          });
          return;
        }
      } catch (e) {}
      resolve(false);
    });
  }

  // The isolated-world bridge (service worker bundles, chrome.storage gh_lang / gh_compact_ui
  // -> page) lives in i18n_bridge.js. Chromium injects each manifest content-script file at most
  // once per frame, so one file cannot serve both the isolated and the MAIN-world entry: with
  // i18n.js in both, only the isolated copy ran and window.GHI18n never reached the page.

  // -------------------------------------------------------------------------
  // Extension Popup (chrome-extension:) & Page MAIN World
  // -------------------------------------------------------------------------
  let initialStoredLang = null;
  try {
    initialStoredLang = localStorage.getItem(STORAGE_KEY_LANG_CACHE);
    localStorage.removeItem(LEGACY_LOCAL_KEY_LANG);
  } catch (e) {}

  function cacheLang(lang) {
    try { localStorage.setItem(STORAGE_KEY_LANG_CACHE, lang); } catch (e) {}
  }

  const useEmbeddedPageBundles = !isExtensionPage && isHoyolabMap;
  const core = createI18n(useEmbeddedPageBundles ? EMBEDDED_PAGE_BUNDLES : null, initialStoredLang);
  let initPromise = null;
  // True only once the user's choice (chrome.storage gh_lang) is known — never from the cache.
  let hasExplicitStoredLang = false;

  function updateDocumentLang(lang) {
    try {
      if (isExtensionPage && typeof document !== 'undefined' && document.documentElement) {
        document.documentElement.lang = lang;
      }
    } catch (e) {}
  }

  async function init() {
    if (initPromise) return initPromise;
    initPromise = (async () => {
      if (isExtensionPage && hasChromeRuntime) {
        const [bundles, stored] = await Promise.all([
          fetchLocaleBundles((rel) => chrome.runtime.getURL(rel)),
          storageGet([STORAGE_KEY_LANG])
        ]);
        const storedCode = normalizeLang(stored[STORAGE_KEY_LANG]);
        hasExplicitStoredLang = Boolean(storedCode);
        const resolved = resolveLanguage(storedCode || initialStoredLang);
        core.setLangSync(resolved);
        core.setBundles(bundles);
        cacheLang(resolved);
        updateDocumentLang(resolved);
        return resolved;
      }

      // MAIN world on HoYoLAB: request push from isolated script (SW-backed) + retry if isolated boots later
      if (isHoyolabMap) {
        const pingIsolated = () => {
          try {
            window.postMessage({
              __gh_ext: true,
              dir: 'to-isolated',
              kind: 'I18N_REQUEST'
            }, window.location.origin);
          } catch (e) {}
        };
        pingIsolated();

        if (!core.isReady()) {
          const bundles = await fetchLocaleBundles(
            (rel) => `chrome-extension://${PINNED_EXT_ID}/${rel}`
          );
          const merged = mergeWithEmbeddedPageBundles(bundles);
          if (!core.isReady() && merged) {
            core.setBundles(merged);
          }
        }
        if (!core.isReady()) {
          setTimeout(() => { if (!core.isReady()) pingIsolated(); }, 300);
          setTimeout(() => { if (!core.isReady()) pingIsolated(); }, 1000);
        }
      } else if (window.location.protocol === 'http:' || window.location.protocol === 'https:') {
        const bundles = await fetchLocaleBundles((rel) => rel);
        if (Object.keys(bundles.vi).length > 0 || Object.keys(bundles.en).length > 0) {
          const resolved = resolveLanguage(initialStoredLang || document.documentElement.lang || DEFAULT_LANG);
          core.setLangSync(resolved);
          core.setBundles(bundles);
          updateDocumentLang(resolved);
        }
      }
      return core.getLang();
    })();
    return initPromise;
  }

  // opts.explicit === false: a default (e.g. the desktop app's language while the user has not
  // picked one) — applied and cached, but neither persisted nor treated as the user's choice.
  async function setLang(nextLang, opts = {}) {
    const explicit = opts.explicit !== false;
    const resolved = core.setLangSync(nextLang);
    if (explicit) hasExplicitStoredLang = true;
    cacheLang(resolved);
    updateDocumentLang(resolved);

    if (explicit && isExtensionPage && hasChromeRuntime && opts.persist !== false) {
      await storageSet({ [STORAGE_KEY_LANG]: resolved });
    } else if (explicit && !isExtensionPage && isHoyolabMap && opts.notifyIsolated !== false) {
      try {
        window.postMessage({
          __gh_ext: true,
          dir: 'to-isolated',
          kind: 'I18N_SET_LANG',
          lang: resolved
        }, window.location.origin);
      } catch (e) {}
    }

    if (isExtensionPage && opts.applyDom !== false) {
      core.applyDom(document);
    }
    return resolved;
  }

  // Receive pushed bundles & live language switches from isolated bridge in MAIN world
  if (!isExtensionPage && isHoyolabMap) {
    const applyIncomingSync = (data) => {
      if (!data || data.kind !== 'I18N_SYNC') return;
      const merged = data.bundles ? mergeWithEmbeddedPageBundles(data.bundles) : null;
      try {
        window.__gh_i18n_last_sync = {
          bundles: merged || (window.__gh_i18n_last_sync && window.__gh_i18n_last_sync.bundles) || null,
          lang: data.lang || (window.__gh_i18n_last_sync && window.__gh_i18n_last_sync.lang) || null
        };
      } catch (err) {}
      if (merged) {
        core.setBundles(merged);
      }
      if (data.lang) {
        // The bridge only sends a language the user picked (chrome.storage gh_lang).
        const resolved = core.setLangSync(data.lang);
        hasExplicitStoredLang = true;
        cacheLang(resolved);
      }
    };
    window.__gh_i18n_main_sync_handler = applyIncomingSync;
    if (window.__gh_i18n_last_sync) {
      applyIncomingSync({ kind: 'I18N_SYNC', ...window.__gh_i18n_last_sync });
    }
    if (!window.__gh_i18n_main_listener) {
      window.__gh_i18n_main_listener = true;
      window.addEventListener('message', (e) => {
        if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-page') {
          return;
        }
        if (e.data.kind === 'I18N_SYNC' && typeof window.__gh_i18n_main_sync_handler === 'function') {
          window.__gh_i18n_main_sync_handler(e.data);
        }
      });
    }
  }

  globalScope.GHI18n = {
    SUPPORTED_LANGS,
    DEFAULT_LANG,
    STORAGE_KEY_LANG,
    init,
    t: core.t,
    getLang: core.getLang,
    setLang,
    applyDom: core.applyDom,
    onChange: core.onChange,
    resolveLanguage: core.resolveLanguage,
    normalizeLang: core.normalizeLang,
    setBundles: core.setBundles,
    isReady: core.isReady,
    hasExplicitPreference: () => hasExplicitStoredLang
  };

  // Auto-kickstart in MAIN world so bundles arrive immediately
  if (!isExtensionPage && isHoyolabMap) {
    init();
  }
})(typeof window !== 'undefined' ? window : globalThis);
