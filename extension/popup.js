'use strict';

function sendBg(msg) {
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendMessage(msg, (response) => {
        const err = chrome.runtime.lastError;
        if (err) {
          resolve({ ok: false, error: err.message });
          return;
        }
        resolve(response || { ok: false, error: 'Empty response' });
      });
    } catch (e) {
      resolve({ ok: false, error: e.message || 'sendMessage failed' });
    }
  });
}

function apiRequest(path, method = 'GET', body = undefined) {
  return sendBg({ kind: 'API_REQUEST', path, method, body });
}

function t(key, params) {
  if (window.GHI18n && typeof window.GHI18n.t === 'function') {
    return window.GHI18n.t(key, params);
  }
  return key;
}

function getActiveLang() {
  if (window.GHI18n && typeof window.GHI18n.getLang === 'function') {
    return window.GHI18n.getLang();
  }
  return 'vi';
}

function notifyMapTabs(payload) {
  try {
    if (typeof chrome === 'undefined' || !chrome.tabs || typeof chrome.tabs.query !== 'function') {
      return;
    }
    const queryUrls = (window.GHMapProviders && Array.isArray(window.GHMapProviders.ALL_TAB_QUERY_URLS))
      ? window.GHMapProviders.ALL_TAB_QUERY_URLS
      : ['https://act.hoyolab.com/*'];
    chrome.tabs.query({ url: queryUrls }, (tabs) => {
      void (chrome.runtime && chrome.runtime.lastError);
      if (!Array.isArray(tabs)) return;
      for (const tab of tabs) {
        if (tab && typeof tab.id === 'number') {
          try {
            chrome.tabs.sendMessage(tab.id, payload, () => {
              void (chrome.runtime && chrome.runtime.lastError);
            });
          } catch (e) {}
        }
      }
    });
  } catch (e) {}
}

function readLocalStorage(keys) {
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

function writeLocalStorage(obj) {
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

function formatRelativeTime(tsVal) {
  if (typeof tsVal !== 'number' || !Number.isFinite(tsVal) || tsVal <= 0) {
    return '';
  }
  const tsMs = tsVal < 1e12 ? tsVal * 1000 : tsVal;
  const diffSec = Math.max(0, Math.floor((Date.now() - tsMs) / 1000));
  if (diffSec < 5) return t('popup.time.just_now');
  if (diffSec < 60) return t('popup.time.seconds_ago', { n: diffSec });
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return t('popup.time.minutes_ago', { n: diffMin });
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return t('popup.time.hours_ago', { n: diffHr });
  try {
    const localeTag = getActiveLang() === 'vi' ? 'vi-VN' : 'en-US';
    return new Date(tsMs).toLocaleTimeString(localeTag, { hour: '2-digit', minute: '2-digit' });
  } catch (e) {
    return '';
  }
}

document.addEventListener('DOMContentLoaded', () => {
  // Header & Tabs
  const versionBadge = document.getElementById('version-badge');
  const btnLangToggle = document.getElementById('btn-lang-toggle');
  const statusPill = document.getElementById('status-pill');
  const statusText = document.getElementById('status-text');
  const tabButtons = Array.from(document.querySelectorAll('.tab-btn'));
  const tabPanels = {
    control: document.getElementById('tab-panel-control'),
    tools: document.getElementById('tab-panel-tools'),
    settings: document.getElementById('tab-panel-settings')
  };
  const uipiWarningBanner = document.getElementById('uipi-warning-banner');

  // Tab 1: Control
  const powerTitle = document.getElementById('power-title');
  const powerDesc = document.getElementById('power-desc');
  const btnPowerToggle = document.getElementById('btn-power-toggle');
  const btnPowerStandby = document.getElementById('btn-power-standby');
  const nativeHostBanner = document.getElementById('native-host-banner');

  const syncFloorBadge = document.getElementById('sync-floor-badge');
  const syncLocation = document.getElementById('sync-location');
  const syncCoordsRow = document.getElementById('sync-coords-row');
  const syncCoords = document.getElementById('sync-coords');
  const syncTime = document.getElementById('sync-time');

  const toggleAutoSync = document.getElementById('toggle-auto-sync');
  const toggleMinimapTracking = document.getElementById('toggle-minimap-tracking');
  const toggleNavHud = document.getElementById('toggle-nav-hud');
  const hudSubGroup = document.getElementById('hud-sub-group');
  const selectHudPosition = document.getElementById('select-hud-position');
  const selectHudGuideZoom = document.getElementById('select-hud-guide-zoom');
  const btnHudEdit = document.getElementById('btn-hud-edit');
  const toggleHudCapture = document.getElementById('toggle-hud-capture');
  const toggleAudioEnabled = document.getElementById('toggle-audio-enabled');
  const audioVolRow = document.getElementById('audio-vol-row');
  const rangeAudioVolume = document.getElementById('range-audio-volume');
  const audioVolumeValue = document.getElementById('audio-volume-value');
  const btnAudioTest = document.getElementById('btn-audio-test');
  const toggleRadarEnabled = document.getElementById('toggle-radar-enabled');
  const toggleCompactUi = document.getElementById('toggle-compact-ui');

  const btnOpenMap = document.getElementById('btn-open-map');
  const btnSwitchGame = document.getElementById('btn-switch-game');

  // Tab 2: Tools (Web Route Companion, Cross-Map Export, LAN Companion QR, Borderless Launch)
  const routeStatusText = document.getElementById('route-status-text');
  const selectMmapOverlayMode = document.getElementById('select-mmap-overlay-mode');
  const btnRouteExport = document.getElementById('btn-route-export');
  const btnRouteImport = document.getElementById('btn-route-import');
  const btnRouteClear = document.getElementById('btn-route-clear');
  const routeImportTray = document.getElementById('route-import-tray');
  const inputRouteFile = document.getElementById('input-route-file');
  const btnRoutePickFile = document.getElementById('btn-route-pick-file');
  const inputRouteUrl = document.getElementById('input-route-url');
  const btnRouteOpenUrl = document.getElementById('btn-route-open-url');
  let cachedRouteStatus = null;
  let routeStatusInFlight = false;
  let activeTabName = 'control';
  const selectExportFormat = document.getElementById('select-export-format');
  const btnExportPins = document.getElementById('btn-export-pins');
  const lanStatusBadge = document.getElementById('lan-status-badge');
  const btnToggleLan = document.getElementById('btn-toggle-lan');
  const lanDetails = document.getElementById('lan-details');
  const lanQrBox = document.getElementById('lan-qr-box');
  const lanPinCode = document.getElementById('lan-pin-code');
  const lanUrlText = document.getElementById('lan-url-text');
  const btnCopyLanUrl = document.getElementById('btn-copy-lan-url');
  const lanClientsText = document.getElementById('lan-clients-text');
  const btnLaunchBorderless = document.getElementById('btn-launch-borderless');

  // Tab 3: Hotkeys & Diagnostics
  const recorders = {
    sync: {
      box: document.getElementById('rec-box-sync'),
      display: document.getElementById('rec-display-sync'),
      badge: document.getElementById('rec-badge-sync'),
      val: 'alt+f11',
      displayText: 'Alt + F11'
    },
    switch: {
      box: document.getElementById('rec-box-switch'),
      display: document.getElementById('rec-display-switch'),
      badge: document.getElementById('rec-badge-switch'),
      val: 'alt+q',
      displayText: 'Alt + Q'
    },
    debug: {
      box: document.getElementById('rec-box-debug'),
      display: document.getElementById('rec-display-debug'),
      badge: document.getElementById('rec-badge-debug'),
      val: 'alt+f12',
      displayText: 'Alt + F12'
    }
  };
  // In-game navigation hotkeys (left-hand Alt-only, zero Shift Sprint conflicts)
  const NAV_HOTKEY_DEFAULTS = {
    nav_mark: ['alt+f', 'Alt + F'],
    nav_undo: ['alt+t', 'Alt + T'],
    nav_guide: ['alt+h', 'Alt + H'],
    nav_next: ['alt+e', 'Alt + E'],
    nav_layout: ['alt+f7', 'Alt + F7'],
    nav_toggle: ['alt+x', 'Alt + X'],
    radar_toggle: ['alt+`', 'Alt + `'],
    nav_unlock: ['alt+f6', 'Alt + F6']
  };
  for (const [navKey, [navVal, navDisp]] of Object.entries(NAV_HOTKEY_DEFAULTS)) {
    const box = document.getElementById(`rec-box-${navKey}`);
    if (!box) continue;
    recorders[navKey] = {
      box,
      display: document.getElementById(`rec-display-${navKey}`),
      badge: document.getElementById(`rec-badge-${navKey}`),
      val: navVal,
      displayText: navDisp
    };
  }
  const HOTKEY_ACTIONS = ['sync', 'switch', 'debug', ...Object.keys(NAV_HOTKEY_DEFAULTS)];
  const presetButtons = Array.from(document.querySelectorAll('.preset-btn[data-target]'));
  const hotkeyBackupsBar = document.getElementById('hotkey-backups-bar');
  const btnSaveHotkeys = document.getElementById('btn-save-hotkeys');
  const diagDrawer = document.getElementById('diag-drawer');
  const btnDiagToggle = document.getElementById('btn-diag-toggle');
  const btnDiagClose = document.getElementById('btn-diag-close');
  const diagCatChips = Array.from(document.querySelectorAll('.diag-cat-chip[data-diag-cat]'));
  const diagNoteInput = document.getElementById('diag-note-input');
  const toggleDiagBrowser = document.getElementById('toggle-diag-browser');
  const btnTriggerDiag = document.getElementById('btn-trigger-diag');
  const diagStatus = document.getElementById('diag-status');
  const diagResultBox = document.getElementById('diag-result-box');
  const diagZipName = document.getElementById('diag-zip-name');
  const diagZipMeta = document.getElementById('diag-zip-meta');
  const btnDiagOpenFolder = document.getElementById('btn-diag-open-folder');
  const btnDiagCopyTemplate = document.getElementById('btn-diag-copy-template');
  const btnHardQuit = document.getElementById('btn-hard-quit');

  // Global feedback & footer
  const feedbackBar = document.getElementById('popup-feedback');
  const footerConnDot = document.getElementById('footer-conn-dot');
  const mapTabStatus = document.getElementById('map-tab-status');
  const donateDrawer = document.getElementById('donate-drawer');
  const btnDonateToggle = document.getElementById('btn-donate-toggle');
  const btnDonateClose = document.getElementById('btn-donate-close');
  const btnDonateMomo = document.getElementById('btn-donate-momo');
  const donateMomoSub = document.getElementById('donate-momo-sub');
  const donateQrWrap = document.getElementById('donate-qr-wrap');
  const donateQrImg = document.getElementById('donate-qr-img');
  const updateDrawer = document.getElementById('update-drawer');
  const btnUpdateClose = document.getElementById('btn-update-close');
  const updateVerCurrent = document.getElementById('update-ver-current');
  const updateVerArrow = document.getElementById('update-ver-arrow');
  const updateVerLatest = document.getElementById('update-ver-latest');
  const updateShaPill = document.getElementById('update-sha-pill');
  const updateStatusText = document.getElementById('update-status-text');
  const updateProgressWrap = document.getElementById('update-progress-wrap');
  const updateProgressFill = document.getElementById('update-progress-fill');
  const updateProgressMeta = document.getElementById('update-progress-meta');
  const updateNotesBox = document.getElementById('update-notes-box');
  const btnUpdatePrimary = document.getElementById('btn-update-primary');
  const btnUpdateGithub = document.getElementById('btn-update-github');
  const selectUpdateMode = document.getElementById('select-update-mode');
  const updateMapStatus = document.getElementById('update-map-status');
  const btnSyncMapData = document.getElementById('btn-sync-map-data');
  const updateUninstallRow = document.getElementById('update-uninstall-row');
  const btnUninstallExtension = document.getElementById('btn-uninstall-extension');

  // Local state
  let currentConnected = false;
  let currentEngineActive = true;
  let currentLastStatus = null;
  let currentLastSync = null;
  let currentUpdateState = null;
  let updatePollTimer = null;
  let updateActionBusy = false;
  let cachedTabCount = 0;
  let powerBusy = false;
  let hotkeysDirty = false;
  let activeRecordTarget = null;
  let activeExtRecord = null;
  let conflictCheckSeq = 0;
  let initialConflictChecked = false;
  let feedbackTimer = null;
  let diagHasCustomStatus = false;
  let selectedDiagCategory = '';
  let lastDiagBundle = null;
  let diagCaptureBusy = false;
  let lanRunning = false;
  let lanInfo = null;
  let lanBusy = false;
  let lanStatusFetchInFlight = false;
  let lastRenderedQrSvg = '';

  function setBtnLabel(btn, text) {
    if (!btn) return;
    const lbl = btn.querySelector('.btn-label');
    if (lbl) {
      lbl.textContent = text;
    } else {
      btn.textContent = text;
    }
  }

  function syncHudSubGroupState() {
    if (!toggleNavHud) return;
    const enabled = Boolean(toggleNavHud.checked);
    if (hudSubGroup) {
      hudSubGroup.classList.toggle('is-disabled', !enabled);
    }
    if (selectHudPosition) selectHudPosition.disabled = !enabled;
    if (selectHudGuideZoom) selectHudGuideZoom.disabled = !enabled;
    if (btnHudEdit) btnHudEdit.disabled = !enabled;
    if (toggleHudCapture) toggleHudCapture.disabled = !enabled;
  }

  function renderLangToggleUI(lang) {
    if (!btnLangToggle) return;
    const active = lang === 'en' ? 'en' : 'vi';
    btnLangToggle.setAttribute('data-lang', active);
    const opts = btnLangToggle.querySelectorAll('.lang-opt');
    for (const opt of opts) {
      opt.classList.toggle('is-active', opt.getAttribute('data-lang-opt') === active);
    }
  }

  function applyLanguageToPopup() {
    const lang = getActiveLang();
    renderLangToggleUI(lang);
    if (window.GHI18n && typeof window.GHI18n.applyDom === 'function') {
      window.GHI18n.applyDom(document);
    }
    if (donateMomoSub && donateQrWrap) {
      donateMomoSub.textContent = donateQrWrap.hidden
        ? t('popup.donate.momo_sub')
        : t('popup.donate.momo_hide');
    }
    document.title = t('popup.title');
    for (const rec of Object.values(recorders)) {
      if (rec.box.classList.contains('is-recording')) {
        rec.display.textContent = t('popup.hotkeys.press_key');
        rec.badge.textContent = t('popup.hotkeys.badge_listening');
      } else if (rec.box.classList.contains('is-dirty')) {
        rec.badge.textContent = t('popup.hotkeys.badge_unsaved');
      } else {
        rec.badge.textContent = t('popup.hotkeys.badge_change');
      }
    }
    if (!diagHasCustomStatus) {
      diagStatus.textContent = t('popup.diag.ready');
    }
    if (!diagCaptureBusy && btnTriggerDiag) {
      setBtnLabel(btnTriggerDiag, t('popup.diag.btn'));
    }
    if (lastDiagBundle && diagZipMeta) {
      renderDiagResultMeta(lastDiagBundle);
    }
    applyState({
      connected: currentConnected,
      engineActive: currentEngineActive,
      statusData: currentLastStatus,
      lastSync: currentLastSync,
      tabCount: cachedTabCount
    });
  }

  function showFeedback(text, variant = 'warn', autoHideMs = 0) {
    if (feedbackTimer) {
      clearTimeout(feedbackTimer);
      feedbackTimer = null;
    }
    feedbackBar.classList.remove('is-error', 'is-success');
    if (!text) {
      feedbackBar.hidden = true;
      feedbackBar.textContent = '';
      return;
    }
    feedbackBar.hidden = false;
    feedbackBar.textContent = text;
    if (variant === 'error') {
      feedbackBar.classList.add('is-error');
    } else if (variant === 'success') {
      feedbackBar.classList.add('is-success');
    }
    if (autoHideMs > 0) {
      feedbackTimer = setTimeout(() => {
        feedbackBar.hidden = true;
        feedbackBar.textContent = '';
        feedbackBar.classList.remove('is-error', 'is-success');
      }, autoHideMs);
    }
  }

  function renderSafeQrSvg(container, svgMarkup) {
    if (!container) return;
    if (!svgMarkup || typeof svgMarkup !== 'string') {
      while (container.firstChild) container.removeChild(container.firstChild);
      lastRenderedQrSvg = '';
      return;
    }
    if (svgMarkup === lastRenderedQrSvg && container.firstChild) {
      return;
    }
    try {
      const parser = new DOMParser();
      const doc = parser.parseFromString(svgMarkup, 'image/svg+xml');
      const root = doc && doc.documentElement;
      if (!root || root.nodeName.toLowerCase() !== 'svg' || doc.querySelector('parsererror')) {
        return;
      }
      const SVG_NS = 'http://www.w3.org/2000/svg';
      const ALLOWED_TAGS = new Set(['svg', 'rect', 'path', 'g']);
      const ALLOWED_ATTRS = new Set([
        'viewbox', 'width', 'height', 'shape-rendering', 'role', 'aria-label',
        'd', 'fill', 'x', 'y', 'rx', 'ry'
      ]);
      const cloneSafe = (node) => {
        const tag = node.nodeName.toLowerCase();
        if (!ALLOWED_TAGS.has(tag)) return null;
        const el = document.createElementNS(SVG_NS, tag);
        if (node.attributes) {
          for (const attr of Array.from(node.attributes)) {
            const nameLower = attr.name.toLowerCase();
            if (ALLOWED_ATTRS.has(nameLower)) {
              const properName = nameLower === 'viewbox' ? 'viewBox' : attr.name;
              el.setAttribute(properName, attr.value);
            }
          }
        }
        for (const child of Array.from(node.childNodes)) {
          if (child.nodeType === 1) {
            const safeChild = cloneSafe(child);
            if (safeChild) el.appendChild(safeChild);
          }
        }
        return el;
      };
      const safeSvg = cloneSafe(root);
      if (safeSvg) {
        while (container.firstChild) container.removeChild(container.firstChild);
        container.appendChild(safeSvg);
        lastRenderedQrSvg = svgMarkup;
      }
    } catch (e) {}
  }

  function renderLanCompanionUI() {
    if (lanStatusBadge) {
      lanStatusBadge.classList.toggle('is-on', lanRunning);
      lanStatusBadge.classList.toggle('is-off', !lanRunning);
      lanStatusBadge.textContent = lanRunning ? t('popup.lan.status_on') : t('popup.lan.status_off');
    }
    if (btnToggleLan && !lanBusy) {
      setBtnLabel(btnToggleLan, lanRunning ? t('popup.lan.stop_btn') : t('popup.lan.start_btn'));
      btnToggleLan.classList.toggle('btn-brass', lanRunning);
      btnToggleLan.classList.toggle('btn-primary', !lanRunning);
      btnToggleLan.disabled = !currentConnected;
    }
    if (lanDetails) {
      lanDetails.hidden = !lanRunning;
    }
    if (lanRunning && lanInfo && typeof lanInfo === 'object') {
      if (lanPinCode) {
        lanPinCode.textContent = lanInfo.pin ? String(lanInfo.pin) : '------';
      }
      if (lanUrlText) {
        const urlStr = lanInfo.url ? String(lanInfo.url) : '';
        lanUrlText.textContent = urlStr;
        lanUrlText.title = urlStr;
      }
      if (lanClientsText) {
        const cnt = typeof lanInfo.client_count === 'number' ? lanInfo.client_count : 0;
        lanClientsText.textContent = t('popup.lan.clients', { count: cnt });
      }
      if (lanQrBox) {
        renderSafeQrSvg(lanQrBox, lanInfo.qr_svg || '');
      }
    } else if (!lanRunning && lanQrBox) {
      renderSafeQrSvg(lanQrBox, '');
    }
  }

  async function fetchLanStatus() {
    if (lanStatusFetchInFlight || !currentConnected) return;
    lanStatusFetchInFlight = true;
    try {
      const res = await apiRequest('/lan_companion', 'POST', { action: 'status' });
      if (res && res.ok && res.data && typeof res.data === 'object') {
        const isOn = Boolean(res.data.running || res.data.is_running);
        lanRunning = isOn;
        lanInfo = isOn ? res.data : null;
        renderLanCompanionUI();
      }
    } finally {
      lanStatusFetchInFlight = false;
    }
  }

  function switchTab(tabName) {
    activeTabName = tabName;
    if (activeRecordTarget || activeExtRecord) {
      stopRecording(null, null, false);
    }
    for (const btn of tabButtons) {
      const active = btn.getAttribute('data-tab') === tabName;
      btn.classList.toggle('is-active', active);
      btn.setAttribute('aria-selected', active ? 'true' : 'false');
    }
    for (const [key, panel] of Object.entries(tabPanels)) {
      if (panel) {
        panel.hidden = key !== tabName;
      }
    }
    if (tabName === 'tools' && currentConnected) {
      refreshRouteStatus();
      if (lanRunning && (!lanInfo || !lanInfo.qr_svg)) fetchLanStatus();
    }
    if (tabName === 'settings') {
      checkHotkeyConflicts();
    }
  }

  function renderTelemetry(lastSync) {
    currentLastSync = lastSync || null;
    const valid = Boolean(
      lastSync &&
      typeof lastSync === 'object' &&
      typeof lastSync.lat === 'number' &&
      typeof lastSync.lng === 'number'
    );

    if (!valid) {
      syncFloorBadge.hidden = true;
      syncFloorBadge.classList.remove('is-underground');
      syncLocation.textContent = t('popup.telemetry.empty');
      syncLocation.classList.add('is-empty');
      syncCoordsRow.hidden = true;
      return;
    }

    const mapName = lastSync.map_name || (lastSync.map_id ? `Map ${lastSync.map_id}` : 'Teyvat');
    const areaPart = lastSync.area_name ? ` • ${lastSync.area_name}` : '';
    syncLocation.textContent = `${mapName}${areaPart}`;
    syncLocation.classList.remove('is-empty');

    syncFloorBadge.hidden = false;
    if (lastSync.is_underground) {
      syncFloorBadge.textContent = t('popup.telemetry.floor_underground', { floor: lastSync.floor || 'B1' });
      syncFloorBadge.classList.add('is-underground');
    } else {
      syncFloorBadge.textContent = (lastSync.floor && lastSync.floor !== 'G')
        ? lastSync.floor
        : t('popup.telemetry.floor_surface');
      syncFloorBadge.classList.remove('is-underground');
    }

    syncCoordsRow.hidden = false;
    syncCoords.textContent = `(${lastSync.lat.toFixed(1)}, ${lastSync.lng.toFixed(1)})`;
    syncTime.textContent = formatRelativeTime(lastSync.ts);
  }

  function renderHotkeysFromStatus(statusData) {
    if (!statusData || typeof statusData !== 'object' || hotkeysDirty || activeRecordTarget || activeExtRecord) {
      return;
    }
    const mappings = [
      { key: 'sync', raw: statusData.hotkey_sync || statusData.hotkey, disp: statusData.display_name_sync || statusData.display_name },
      { key: 'switch', raw: statusData.hotkey_switch, disp: statusData.display_name_switch },
      { key: 'debug', raw: statusData.hotkey_debug, disp: statusData.display_name_debug },
      ...Object.keys(NAV_HOTKEY_DEFAULTS).map((k) => ({
        key: k,
        raw: statusData[`hotkey_${k}`],
        disp: statusData[`display_name_${k}`]
      }))
    ];
    for (const m of mappings) {
      const rec = recorders[m.key];
      if (!rec) continue;
      if (m.raw) rec.val = String(m.raw).trim().toLowerCase();
      if (m.disp) {
        rec.displayText = String(m.disp).trim();
        rec.display.textContent = rec.displayText;
      }
      rec.box.classList.remove('is-dirty');
      rec.badge.textContent = t('popup.hotkeys.badge_change');
    }
    if (!initialConflictChecked && currentConnected) {
      initialConflictChecked = true;
      checkHotkeyConflicts();
    }
  }

  let cachedActiveProvider = null;
  const selectMapProvider = document.getElementById('select-map-provider');

  function applyState({ connected, engineActive, statusData, lastSync, tabCount, activeProvider }) {
    if (typeof connected === 'boolean') currentConnected = connected;
    if (typeof engineActive === 'boolean') currentEngineActive = engineActive;
    if (statusData !== undefined) currentLastStatus = statusData;
    if (typeof tabCount === 'number') cachedTabCount = tabCount;
    if (typeof activeProvider === 'string' && activeProvider) {
      cachedActiveProvider = activeProvider;
    } else if (tabCount === 0) {
      cachedActiveProvider = null;
    }

    const baseTabText = t('popup.footer.map_tabs', { count: cachedTabCount });
    if (cachedTabCount > 0 && cachedActiveProvider && window.GHMapProviders) {
      const pMeta = window.GHMapProviders.getProvider(cachedActiveProvider);
      mapTabStatus.textContent = `${pMeta.shortName} · ${baseTabText}`;
    } else {
      mapTabStatus.textContent = baseTabText;
    }

    const verRaw = (currentLastStatus && currentLastStatus.version) ? String(currentLastStatus.version) : '5.7.0';
    const curVerText = verRaw.startsWith('v') ? verRaw : `v${verRaw}`;
    const updSummary = (currentUpdateState && typeof currentUpdateState === 'object')
      ? currentUpdateState
      : ((currentLastStatus && currentLastStatus.app_update && typeof currentLastStatus.app_update === 'object')
          ? currentLastStatus.app_update
          : null);
    const updPhase = (updSummary && typeof updSummary.phase === 'string') ? updSummary.phase : 'idle';
    const hasUpdate = Boolean(currentConnected && updSummary && updSummary.has_update && updSummary.latest_version);
    const latestClean = hasUpdate ? String(updSummary.latest_version).replace(/^v/i, '') : '';
    const isChecking = Boolean(currentConnected && updPhase === 'checking');
    const isDownloading = Boolean(currentConnected && (updPhase === 'downloading' || updPhase === 'verifying'));
    const isReady = Boolean(hasUpdate && (updPhase === 'ready_to_install' || updSummary.verified_ready));
    const dlPct = (updSummary && typeof updSummary.progress_pct === 'number')
      ? Math.max(0, Math.min(100, Math.round(updSummary.progress_pct)))
      : 0;
    if (versionBadge) {
      versionBadge.classList.remove('has-update', 'is-checking', 'is-downloading', 'is-ready');
      if (isDownloading) {
        versionBadge.classList.add('is-downloading');
        setBtnLabel(versionBadge, `${dlPct}%`);
        versionBadge.title = t('popup.update.badge_downloading_title', {
          latest: latestClean || curVerText.replace(/^v/i, ''),
          pct: dlPct
        });
      } else if (isChecking) {
        versionBadge.classList.add('is-checking');
        setBtnLabel(versionBadge, curVerText);
        versionBadge.title = t('popup.update.status_checking');
      } else if (isReady) {
        versionBadge.classList.add('is-ready');
        setBtnLabel(versionBadge, `v${latestClean}`);
        versionBadge.title = t('popup.update.badge_ready_title', { latest: latestClean });
      } else if (hasUpdate) {
        versionBadge.classList.add('has-update');
        setBtnLabel(versionBadge, `v${latestClean}`);
        versionBadge.title = t('popup.update.badge_new_title', { latest: latestClean });
      } else {
        setBtnLabel(versionBadge, curVerText);
        versionBadge.title = t('popup.update.badge_title');
      }
    }
    renderUpdateDrawerUI();

    statusPill.classList.remove('is-online', 'is-standby', 'is-offline');
    if (footerConnDot) footerConnDot.classList.remove('is-online', 'is-standby', 'is-offline');
    if (currentConnected && currentEngineActive) {
      statusPill.classList.add('is-online');
      if (footerConnDot) footerConnDot.classList.add('is-online');
      statusText.textContent = t('popup.status.active');
    } else if (currentConnected && !currentEngineActive) {
      statusPill.classList.add('is-standby');
      if (footerConnDot) footerConnDot.classList.add('is-standby');
      statusText.textContent = t('popup.status.standby');
    } else {
      statusPill.classList.add('is-offline');
      if (footerConnDot) footerConnDot.classList.add('is-offline');
      statusText.textContent = t('popup.status.offline');
    }

    if (!powerBusy) {
      if (btnPowerToggle) btnPowerToggle.classList.remove('is-active', 'is-busy');
      if (btnPowerStandby) btnPowerStandby.classList.remove('is-active', 'is-busy');
      if (btnHardQuit) btnHardQuit.classList.remove('is-active', 'is-busy');
      setBtnLabel(btnPowerToggle, t('popup.power.active.label'));
      if (btnPowerStandby) setBtnLabel(btnPowerStandby, t('popup.power.standby.label'));
      if (btnHardQuit) setBtnLabel(btnHardQuit, t('popup.power.quit.label'));

      if (currentConnected && currentEngineActive) {
        powerTitle.textContent = t('popup.power.active.title');
        powerDesc.textContent = t('popup.power.active.desc');
        if (btnPowerToggle) {
          btnPowerToggle.classList.add('is-active');
          btnPowerToggle.setAttribute('aria-pressed', 'true');
          btnPowerToggle.disabled = false;
        }
        if (btnPowerStandby) {
          btnPowerStandby.setAttribute('aria-pressed', 'false');
          btnPowerStandby.disabled = false;
        }
        if (btnHardQuit) {
          btnHardQuit.setAttribute('aria-pressed', 'false');
          btnHardQuit.disabled = false;
        }
        nativeHostBanner.hidden = true;
      } else if (currentConnected && !currentEngineActive) {
        powerTitle.textContent = t('popup.power.standby.title');
        powerDesc.textContent = t('popup.power.standby.desc');
        if (btnPowerToggle) {
          btnPowerToggle.setAttribute('aria-pressed', 'false');
          btnPowerToggle.disabled = false;
        }
        if (btnPowerStandby) {
          btnPowerStandby.classList.add('is-active');
          btnPowerStandby.setAttribute('aria-pressed', 'true');
          btnPowerStandby.disabled = false;
        }
        if (btnHardQuit) {
          btnHardQuit.setAttribute('aria-pressed', 'false');
          btnHardQuit.disabled = false;
        }
        nativeHostBanner.hidden = true;
      } else {
        powerTitle.textContent = t('popup.power.offline.title');
        powerDesc.textContent = t('popup.power.offline.desc');
        if (btnPowerToggle) {
          btnPowerToggle.setAttribute('aria-pressed', 'false');
          btnPowerToggle.disabled = false;
        }
        if (btnPowerStandby) {
          btnPowerStandby.setAttribute('aria-pressed', 'false');
          btnPowerStandby.disabled = true;
        }
        if (btnHardQuit) {
          btnHardQuit.classList.add('is-active');
          btnHardQuit.setAttribute('aria-pressed', 'true');
          btnHardQuit.disabled = true;
        }
      }
    }

    const uipiMismatch = Boolean(
      currentConnected &&
      currentLastStatus &&
      (currentLastStatus.uipi_mismatch === true || currentLastStatus.uipi_blocked === true)
    );
    if (uipiWarningBanner) {
      uipiWarningBanner.hidden = !uipiMismatch;
    }

    if (currentLastStatus && typeof currentLastStatus === 'object') {
      if (typeof currentLastStatus.auto_sync_m === 'boolean') {
        toggleAutoSync.checked = currentLastStatus.auto_sync_m;
      }
      if (toggleMinimapTracking && typeof currentLastStatus.minimap_tracking === 'boolean') {
        toggleMinimapTracking.checked = currentLastStatus.minimap_tracking;
      }
      if (toggleNavHud && typeof currentLastStatus.nav_hud_enabled === 'boolean') {
        toggleNavHud.checked = currentLastStatus.nav_hud_enabled;
      }
      syncHudSubGroupState();
      if (selectHudPosition && typeof currentLastStatus.nav_hud_position === 'string' && document.activeElement !== selectHudPosition) {
        const pos = currentLastStatus.nav_hud_position;
        if (Array.from(selectHudPosition.options).some((o) => o.value === pos)) {
          selectHudPosition.value = pos;
        }
      }
      if (btnHudEdit) {
        btnHudEdit.classList.toggle('is-active', currentLastStatus.nav_hud_edit_mode === true);
      }
      if (toggleHudCapture && typeof currentLastStatus.nav_hud_capture_visible === 'boolean') {
        toggleHudCapture.checked = currentLastStatus.nav_hud_capture_visible;
      }
      if (selectHudGuideZoom && typeof currentLastStatus.nav_hud_guide_zoom_mode === 'string' && document.activeElement !== selectHudGuideZoom) {
        const zmode = currentLastStatus.nav_hud_guide_zoom_mode;
        if (Array.from(selectHudGuideZoom.options).some((o) => o.value === zmode)) {
          selectHudGuideZoom.value = zmode;
        }
      }
      if (selectMmapOverlayMode && typeof currentLastStatus.mmap_overlay_mode === 'string' && document.activeElement !== selectMmapOverlayMode) {
        const ovMode = currentLastStatus.mmap_overlay_mode;
        if (Array.from(selectMmapOverlayMode.options).some((o) => o.value === ovMode)) {
          selectMmapOverlayMode.value = ovMode;
        }
      }
      if (toggleAudioEnabled && typeof currentLastStatus.audio_enabled === 'boolean') {
        toggleAudioEnabled.checked = currentLastStatus.audio_enabled;
      }
      if (rangeAudioVolume && typeof currentLastStatus.audio_volume === 'number' && document.activeElement !== rangeAudioVolume) {
        const clampedVol = Math.max(0, Math.min(100, Math.round(currentLastStatus.audio_volume)));
        rangeAudioVolume.value = String(clampedVol);
        if (audioVolumeValue) {
          audioVolumeValue.textContent = `${clampedVol}%`;
        }
      }
      if (audioVolRow && toggleAudioEnabled) {
        audioVolRow.classList.toggle('is-muted', !toggleAudioEnabled.checked);
      }
      if (toggleRadarEnabled && typeof currentLastStatus.radar_enabled === 'boolean') {
        toggleRadarEnabled.checked = currentLastStatus.radar_enabled;
      }
      renderHotkeysFromStatus(currentLastStatus);

      if (!lanBusy) {
        const nextLanOn = Boolean(
          currentConnected && (
            currentLastStatus.lan_companion_running === true ||
            (currentLastStatus.lan_companion && (currentLastStatus.lan_companion.running || currentLastStatus.lan_companion.is_running))
          )
        );
        lanRunning = nextLanOn;
        if (nextLanOn) {
          if (currentLastStatus.lan_companion && currentLastStatus.lan_companion.qr_svg) {
            lanInfo = currentLastStatus.lan_companion;
          } else if (!lanInfo || !lanInfo.qr_svg) {
            fetchLanStatus();
          }
        } else {
          lanInfo = null;
        }
      }
    } else if (!currentConnected && !lanBusy) {
      lanRunning = false;
      lanInfo = null;
    }

    renderLanCompanionUI();
    renderRouteCardStatus(cachedRouteStatus);

    if (lastSync !== undefined) {
      renderTelemetry(lastSync);
    }
  }

  async function refreshFromBg() {
    const bgState = await sendBg({ kind: 'GET_STATE' });
    if (bgState && typeof bgState === 'object') {
      if (bgState.lastUpdateState && typeof bgState.lastUpdateState === 'object') {
        currentUpdateState = bgState.lastUpdateState;
      }
      applyState({
        connected: Boolean(bgState.connected),
        engineActive: typeof bgState.engineActive === 'boolean' ? bgState.engineActive : true,
        statusData: bgState.lastStatus || null,
        lastSync: bgState.lastSync !== undefined ? bgState.lastSync : currentLastSync,
        tabCount: typeof bgState.tabCount === 'number' ? bgState.tabCount : cachedTabCount,
        activeProvider: bgState.activeProvider || (bgState.lastStatus && bgState.lastStatus.active_map_provider) || null
      });
      if (activeTabName === 'tools' && currentConnected) {
        refreshRouteStatus();
      }
    }
    return bgState;
  }

  async function initPopup() {
    if (window.GHI18n && typeof window.GHI18n.init === 'function') {
      await window.GHI18n.init();
    }
    const storedPrefs = await readLocalStorage(['gh_compact_ui', 'gh_export_format', 'gh_preferred_provider']);
    if (toggleCompactUi) {
      toggleCompactUi.checked = Boolean(storedPrefs.gh_compact_ui);
    }
    if (selectMapProvider && typeof storedPrefs.gh_preferred_provider === 'string') {
      const allowed = ['hoyolab', 'kongying', 'appsample'];
      if (allowed.includes(storedPrefs.gh_preferred_provider)) {
        selectMapProvider.value = storedPrefs.gh_preferred_provider;
      }
    }
    if (selectExportFormat && typeof storedPrefs.gh_export_format === 'string') {
      const valid = ['kongying', 'appsample', 'genshinmap', 'canonical'];
      if (valid.includes(storedPrefs.gh_export_format)) {
        selectExportFormat.value = storedPrefs.gh_export_format;
      }
    }
    applyLanguageToPopup();

    const bgState = await refreshFromBg();
    const res = await apiRequest('/status', 'GET');
    if (res && res.ok && res.data) {
      if (window.GHI18n) {
        if (!window.GHI18n.hasExplicitPreference()) {
          if (res.data.language) {
            await window.GHI18n.setLang(res.data.language, { explicit: false });
            applyLanguageToPopup();
          }
        } else {
          const extLang = getActiveLang();
          const desktopSetting = typeof res.data.language_setting === 'string'
            ? res.data.language_setting.toLowerCase()
            : 'auto';
          if (desktopSetting === 'auto' && res.data.language && res.data.language !== extLang) {
            apiRequest('/set_language', 'POST', { language: extLang }).catch(() => {});
          }
        }
      }
      const nextEngine = typeof res.data.engine_active === 'boolean' ? res.data.engine_active : true;
      const nextSync = Object.prototype.hasOwnProperty.call(res.data, 'last_sync')
        ? res.data.last_sync
        : (bgState ? bgState.lastSync : null);
      applyState({
        connected: true,
        engineActive: nextEngine,
        statusData: res.data,
        lastSync: nextSync,
        tabCount: cachedTabCount
      });
      refreshRouteStatus();
    } else {
      applyState({
        connected: false,
        engineActive: false,
        statusData: (bgState && bgState.lastStatus) || null,
        lastSync: (bgState && bgState.lastSync) || null,
        tabCount: cachedTabCount
      });
      renderRouteCardStatus({ active: false });
    }
  }

  // Smart Conflict Advisor + NVIDIA Overlay Remapper (Tab 3)
  function collectCurrentBindings() {
    const bindings = {};
    for (const act of HOTKEY_ACTIONS) {
      const rec = recorders[act];
      if (rec && rec.val) {
        bindings[act] = String(rec.val).trim().toLowerCase();
      }
    }
    return bindings;
  }

  function buildLocalDuplicateResults(bindings) {
    const byKey = new Map();
    for (const [act, hk] of Object.entries(bindings || {})) {
      if (!hk) continue;
      if (!byKey.has(hk)) byKey.set(hk, []);
      byKey.get(hk).push(act);
    }
    const actions = {};
    for (const [act, hk] of Object.entries(bindings || {})) {
      const peers = (byKey.get(hk) || []).filter((x) => x !== act);
      const conflicts = peers.map((otherAct) => ({
        kind: 'internal',
        severity: 'error',
        app_id: 'genshin_helper',
        app_name: 'Genshin Helper',
        shortcut_id: otherAct,
        can_remap: false,
        message: t('popup.hotkeys.duplicate_error', { key: hk.toUpperCase() })
      }));
      actions[act] = { hotkey: hk, ok: conflicts.length === 0, severity: conflicts.length ? 'error' : 'ok', conflicts, suggestions: [] };
    }
    return { ok: true, actions, active_backups: [] };
  }

  function renderBackupsBar(backups) {
    if (!hotkeyBackupsBar) return;
    hotkeyBackupsBar.textContent = '';
    const list = Array.isArray(backups) ? backups : [];
    if (list.length === 0) {
      hotkeyBackupsBar.hidden = true;
      return;
    }
    hotkeyBackupsBar.hidden = false;

    const titleEl = document.createElement('div');
    titleEl.className = 'backups-title';
    titleEl.textContent = t('popup.hotkeys.backups_title');
    hotkeyBackupsBar.appendChild(titleEl);

    const lang = getActiveLang();
    for (const b of list) {
      const row = document.createElement('div');
      row.className = 'backup-row';

      const desc = (lang === 'en' ? b.desc_en : b.desc_vi) || b.shortcut_id || '';
      const origDisp = b.original_display || b.original_hotkey || '';
      const curDisp = b.current_display || b.current_hotkey || '';
      const infoEl = document.createElement('span');
      infoEl.className = 'backup-info';
      infoEl.textContent = `${b.app_name || 'NVIDIA'} (${desc}): ${origDisp} -> ${curDisp}`;

      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'btn-backup-restore';
      btn.textContent = t('popup.hotkeys.restore_btn', { old_key: origDisp });
      btn.addEventListener('click', async () => {
        btn.disabled = true;
        btn.textContent = t('popup.hotkeys.restoring');
        const res = await apiRequest('/restore_external_hotkey', 'POST', {
          app_id: b.app_id,
          shortcut_id: b.shortcut_id
        });
        if (res && res.ok && res.data && res.data.ok) {
          showFeedback(
            t('popup.hotkeys.restore_ok', {
              old_key: res.data.restored_display || origDisp,
              app: b.app_name || 'NVIDIA Overlay'
            }),
            'success',
            3200
          );
          checkHotkeyConflicts();
        } else {
          btn.disabled = false;
          btn.textContent = t('popup.hotkeys.restore_btn', { old_key: origDisp });
          const errMsg = (res && res.data && res.data.error) || (res && res.error) || 'Error';
          showFeedback(t('popup.hotkeys.restore_err', { error: errMsg }), 'error', 3600);
        }
      });

      row.appendChild(infoEl);
      row.appendChild(btn);
      hotkeyBackupsBar.appendChild(row);
    }
  }

  function renderConflictAnalysis(data) {
    const actions = (data && data.actions && typeof data.actions === 'object') ? data.actions : {};
    for (const act of HOTKEY_ACTIONS) {
      const groupEl = document.querySelector(`.recorder-group[data-group="${act}"]`);
      if (!groupEl) continue;
      const oldPanel = groupEl.querySelector('.hotkey-conflict-panel');
      const analysis = actions[act];
      const conflicts = (analysis && Array.isArray(analysis.conflicts)) ? analysis.conflicts : [];
      if (conflicts.length === 0) {
        groupEl.classList.remove('has-conflict');
        if (oldPanel) oldPanel.remove();
        continue;
      }

      groupEl.classList.add('has-conflict');
      const panel = oldPanel || document.createElement('div');
      panel.className = 'hotkey-conflict-panel';
      panel.textContent = '';

      const suggestions = Array.isArray(analysis.suggestions) ? analysis.suggestions : [];
      for (const c of conflicts) {
        const item = document.createElement('div');
        const sevClass = c.severity === 'warn' ? 'is-warn' : (c.severity === 'info' ? 'is-info' : 'is-error');
        item.className = `conflict-item ${sevClass}`;

        const msgEl = document.createElement('div');
        msgEl.className = 'conflict-msg';
        msgEl.textContent = c.message || '';
        item.appendChild(msgEl);

        if (c.can_remap === true) {
          const remapBox = document.createElement('div');
          remapBox.className = 'conflict-remap-box';

          const lbl = document.createElement('div');
          lbl.className = 'conflict-remap-label';
          lbl.textContent = t('popup.hotkeys.ext_move_label', {
            app: c.app_name || 'External App',
            action: c.shortcut_label || c.shortcut_id || '',
            old_key: c.current_external_display || c.current_external_hotkey || ''
          });
          remapBox.appendChild(lbl);

          const defaultExt = suggestions.find(
            (s) => s && s.hotkey && !String(s.hotkey).includes('mouse') && !['middle', 'right'].includes(String(s.hotkey))
          ) || null;
          let chosenExtKey = defaultExt ? defaultExt.hotkey : '';
          let chosenExtDisp = defaultExt ? (defaultExt.display_name || defaultExt.hotkey) : t('popup.hotkeys.ext_custom_placeholder');

          const ctrlRow = document.createElement('div');
          ctrlRow.className = 'conflict-remap-controls';

          const chipBtn = document.createElement('button');
          chipBtn.type = 'button';
          chipBtn.className = 'remap-target-chip';
          chipBtn.textContent = chosenExtDisp;
          chipBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            startExternalRecording(chipBtn, chosenExtDisp, (val, disp) => {
              chosenExtKey = val;
              chosenExtDisp = disp;
            });
          });

          const claimBtn = document.createElement('button');
          claimBtn.type = 'button';
          claimBtn.className = 'btn-conflict-remap';
          const claimLabel = t('popup.hotkeys.ext_claim_btn', { app: c.app_name || 'NVIDIA' });
          claimBtn.textContent = claimLabel;
          claimBtn.addEventListener('click', async () => {
            if (activeRecordTarget || activeExtRecord) {
              stopRecording(null, null, false);
            }
            if (!chosenExtKey) {
              showFeedback(t('popup.hotkeys.ext_need_key'), 'warn');
              return;
            }
            claimBtn.disabled = true;
            claimBtn.textContent = t('popup.hotkeys.ext_remapping');
            const rec = recorders[act];
            const res = await apiRequest('/remap_external_hotkey', 'POST', {
              app_id: c.app_id,
              shortcut_id: c.shortcut_id,
              new_external_hotkey: chosenExtKey,
              claim_action: act,
              claim_hotkey: (rec && rec.val) || '',
              bindings: collectCurrentBindings(),
              lang: getActiveLang()
            });
            claimBtn.disabled = false;
            claimBtn.textContent = claimLabel;

            if (res && res.ok && res.data && res.data.ok) {
              if (res.data.claim_updated && rec) {
                const cu = res.data.claim_updated;
                rec.val = cu.hotkey || rec.val;
                rec.displayText = cu.display_name || rec.displayText;
                rec.display.textContent = rec.displayText;
                rec.box.classList.remove('is-dirty');
                rec.badge.textContent = t('popup.hotkeys.badge_change');
                hotkeysDirty = Object.values(recorders).some((r) => r.box.classList.contains('is-dirty'));
              }
              showFeedback(
                t('popup.hotkeys.ext_remap_ok', {
                  app: c.app_name || 'NVIDIA Overlay',
                  new_key: res.data.new_display || chosenExtDisp || chosenExtKey
                }),
                'success',
                4500
              );
              checkHotkeyConflicts();
            } else {
              const errMsg = (res && res.data && res.data.error) || (res && res.error) || 'Unknown error';
              showFeedback(t('popup.hotkeys.ext_remap_err', { error: errMsg }), 'error', 4200);
            }
          });

          ctrlRow.appendChild(chipBtn);
          ctrlRow.appendChild(claimBtn);
          remapBox.appendChild(ctrlRow);
          item.appendChild(remapBox);
        }

        panel.appendChild(item);
      }

      if (suggestions.length > 0 && recorders[act]) {
        const sugRow = document.createElement('div');
        sugRow.className = 'conflict-suggest-row';
        const sugLbl = document.createElement('span');
        sugLbl.className = 'conflict-suggest-label';
        sugLbl.textContent = t('popup.hotkeys.suggest_switch');
        sugRow.appendChild(sugLbl);

        for (const sug of suggestions) {
          if (!sug || !sug.hotkey) continue;
          const pill = document.createElement('button');
          pill.type = 'button';
          pill.className = 'conflict-suggest-pill';
          pill.textContent = sug.display_name || sug.hotkey;
          pill.addEventListener('click', () => {
            if (activeRecordTarget || activeExtRecord) {
              stopRecording(null, null, false);
            }
            const rec = recorders[act];
            rec.val = sug.hotkey;
            rec.displayText = sug.display_name || sug.hotkey;
            rec.display.textContent = rec.displayText;
            rec.box.classList.add('is-dirty');
            rec.badge.textContent = t('popup.hotkeys.badge_unsaved');
            hotkeysDirty = true;
            checkHotkeyConflicts();
          });
          sugRow.appendChild(pill);
        }
        panel.appendChild(sugRow);
      }

      if (!oldPanel) {
        groupEl.appendChild(panel);
      }
    }
    renderBackupsBar(data && data.active_backups);
  }

  async function checkHotkeyConflicts() {
    const seq = ++conflictCheckSeq;
    const bindings = collectCurrentBindings();
    const res = await apiRequest('/check_hotkey_conflicts', 'POST', {
      bindings,
      lang: getActiveLang()
    });
    if (seq !== conflictCheckSeq) return;
    if (res && res.ok && res.data && res.data.actions) {
      renderConflictAnalysis(res.data);
    } else {
      renderConflictAnalysis(buildLocalDuplicateResults(bindings));
    }
  }

  // Hotkey Recorder Logic (Keyboard + Preset Pills)
  function stopRecording(newVal, newDisplay, markDirty = true) {
    if (activeExtRecord) {
      const ext = activeExtRecord;
      activeExtRecord = null;
      removeRecordingListeners();
      ext.chipEl.classList.remove('is-recording');
      if (newVal && newDisplay) {
        ext.displayText = newDisplay;
        ext.chipEl.textContent = newDisplay;
        if (typeof ext.onDone === 'function') {
          ext.onDone(newVal, newDisplay);
        }
      } else {
        ext.chipEl.textContent = ext.displayText;
      }
      return;
    }
    if (!activeRecordTarget) return;
    const target = activeRecordTarget;
    const rec = recorders[target];
    activeRecordTarget = null;
    removeRecordingListeners();
    if (!rec) return;

    rec.box.classList.remove('is-recording');
    if (newVal && newDisplay) {
      rec.val = newVal;
      rec.displayText = newDisplay;
      rec.display.textContent = newDisplay;
      if (markDirty) {
        rec.box.classList.add('is-dirty');
        rec.badge.textContent = t('popup.hotkeys.badge_unsaved');
        hotkeysDirty = true;
        checkHotkeyConflicts();
      } else {
        rec.box.classList.remove('is-dirty');
        rec.badge.textContent = t('popup.hotkeys.badge_change');
      }
    } else {
      rec.display.textContent = rec.displayText;
      rec.badge.textContent = rec.box.classList.contains('is-dirty')
        ? t('popup.hotkeys.badge_unsaved')
        : t('popup.hotkeys.badge_change');
    }
  }

  function startRecording(target) {
    if (activeRecordTarget || activeExtRecord) {
      stopRecording(null, null, false);
    }
    const rec = recorders[target];
    if (!rec) return;
    activeRecordTarget = target;
    rec.box.classList.add('is-recording');
    rec.display.textContent = t('popup.hotkeys.press_key');
    rec.badge.textContent = t('popup.hotkeys.badge_listening');
    rec.box.focus();
    addRecordingListeners();
  }

  function startExternalRecording(chipEl, currentDisplay, onDone) {
    if (activeRecordTarget || activeExtRecord) {
      stopRecording(null, null, false);
    }
    activeExtRecord = { chipEl, displayText: currentDisplay, onDone };
    chipEl.classList.add('is-recording');
    chipEl.textContent = t('popup.hotkeys.press_key');
    chipEl.focus();
    addRecordingListeners();
  }

  function onRecordKeyDown(e) {
    if (!activeRecordTarget && !activeExtRecord) return;
    e.preventDefault();
    e.stopPropagation();

    if (e.key === 'Escape') {
      stopRecording(null, null, false);
      return;
    }

    if (['Control', 'Alt', 'Shift', 'Meta', 'OS'].includes(e.key)) {
      const modsPreview = [];
      if (e.ctrlKey) modsPreview.push('Ctrl');
      if (e.altKey) modsPreview.push('Alt');
      if (e.shiftKey) modsPreview.push('Shift');
      const targetEl = activeExtRecord
        ? activeExtRecord.chipEl
        : (recorders[activeRecordTarget] ? recorders[activeRecordTarget].display : null);
      if (targetEl) {
        targetEl.textContent = `${modsPreview.join(' + ')} + ...`;
      }
      return;
    }

    const modsVal = [];
    const modsDisplay = [];
    if (e.ctrlKey) { modsVal.push('ctrl'); modsDisplay.push('Ctrl'); }
    if (e.altKey) { modsVal.push('alt'); modsDisplay.push('Alt'); }
    if (e.shiftKey) { modsVal.push('shift'); modsDisplay.push('Shift'); }

    let keyName = (e.key || '').toLowerCase();
    let keyDisplayName = (e.key || '').toUpperCase();

    if (e.code && e.code.startsWith('F') && !isNaN(parseInt(e.code.slice(1), 10))) {
      keyName = e.code.toLowerCase();
      keyDisplayName = e.code.toUpperCase();
    } else if (e.code === 'Space') {
      keyName = 'space';
      keyDisplayName = 'Space';
    } else if (e.code === 'Backquote') {
      keyName = '`';
      keyDisplayName = '`';
    } else if (e.code && e.code.startsWith('Key')) {
      keyName = e.code.slice(3).toLowerCase();
      keyDisplayName = e.code.slice(3).toUpperCase();
    } else if (e.code && e.code.startsWith('Digit')) {
      keyName = e.code.slice(5);
      keyDisplayName = keyName;
    } else if (keyName === ' ') {
      keyName = 'space';
      keyDisplayName = 'Space';
    }

    if (!keyName) return;
    modsVal.push(keyName);
    modsDisplay.push(keyDisplayName);

    stopRecording(modsVal.join('+'), modsDisplay.join(' + '), true);
  }

  function onRecordMouseDown(e) {
    if (activeExtRecord) {
      if (e.target !== activeExtRecord.chipEl) {
        stopRecording(null, null, false);
      }
      return;
    }
    if (!activeRecordTarget) return;
    if (e.button === 1 || e.button === 2 || e.button === 3 || e.button === 4) {
      e.preventDefault();
      e.stopPropagation();
      const map = {
        1: { val: 'middle', name: t('popup.hotkeys.mouse_middle') },
        2: { val: 'right', name: t('popup.hotkeys.mouse_right') },
        3: { val: 'mouse4', name: t('popup.hotkeys.mouse4') },
        4: { val: 'mouse5', name: t('popup.hotkeys.mouse5') }
      };
      const btn = map[e.button];
      if (!btn) return;
      const modsVal = [];
      const modsDisplay = [];
      if (e.ctrlKey) { modsVal.push('ctrl'); modsDisplay.push('Ctrl'); }
      if (e.altKey) { modsVal.push('alt'); modsDisplay.push('Alt'); }
      if (e.shiftKey) { modsVal.push('shift'); modsDisplay.push('Shift'); }
      modsVal.push(btn.val);
      modsDisplay.push(btn.name);
      stopRecording(modsVal.join('+'), modsDisplay.join(' + '), true);
    } else if (e.target && typeof e.target.closest === 'function') {
      const insideActiveBox = e.target.closest('.recorder-box') === recorders[activeRecordTarget].box;
      if (!insideActiveBox) {
        stopRecording(null, null, false);
      }
    }
  }

  function onRecordContextMenu(e) {
    if (activeRecordTarget) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  function addRecordingListeners() {
    window.addEventListener('keydown', onRecordKeyDown, { capture: true });
    window.addEventListener('mousedown', onRecordMouseDown, { capture: true });
    window.addEventListener('auxclick', onRecordMouseDown, { capture: true });
    window.addEventListener('contextmenu', onRecordContextMenu, { capture: true });
  }

  function removeRecordingListeners() {
    window.removeEventListener('keydown', onRecordKeyDown, { capture: true });
    window.removeEventListener('mousedown', onRecordMouseDown, { capture: true });
    window.removeEventListener('auxclick', onRecordMouseDown, { capture: true });
    setTimeout(() => {
      window.removeEventListener('contextmenu', onRecordContextMenu, { capture: true });
    }, 200);
  }

  for (const [target, rec] of Object.entries(recorders)) {
    rec.box.addEventListener('click', (e) => {
      e.stopPropagation();
      if (activeRecordTarget !== target) {
        startRecording(target);
      }
    });
    rec.box.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        e.stopPropagation();
        if (activeRecordTarget !== target) {
          startRecording(target);
        }
      }
    });
  }

  for (const btn of presetButtons) {
    btn.addEventListener('click', () => {
      const target = btn.getAttribute('data-target');
      const key = btn.getAttribute('data-key');
      const labelKey = btn.getAttribute('data-label-i18n');
      const label = labelKey ? t(labelKey) : (btn.getAttribute('data-label') || btn.textContent.trim());
      const rec = recorders[target];
      if (!rec || !key) return;
      if (activeRecordTarget || activeExtRecord) {
        stopRecording(null, null, false);
      }
      rec.val = key;
      rec.displayText = label;
      rec.display.textContent = label;
      rec.box.classList.add('is-dirty');
      rec.badge.textContent = t('popup.hotkeys.badge_unsaved');
      hotkeysDirty = true;
      checkHotkeyConflicts();
    });
  }

  // Event Listeners
  if (btnLangToggle) {
    btnLangToggle.addEventListener('click', async (e) => {
      const clickedOpt = e.target && typeof e.target.closest === 'function'
        ? e.target.closest('.lang-opt')
        : null;
      const curLang = getActiveLang();
      const targetLang = clickedOpt
        ? clickedOpt.getAttribute('data-lang-opt')
        : (curLang === 'vi' ? 'en' : 'vi');
      if (!targetLang || targetLang === curLang) return;

      if (window.GHI18n && typeof window.GHI18n.setLang === 'function') {
        await window.GHI18n.setLang(targetLang);
      }
      applyLanguageToPopup();
      notifyMapTabs({ kind: 'GH_SET_LANG', lang: targetLang });
      if (currentConnected) {
        apiRequest('/set_language', 'POST', { language: targetLang }).catch(() => {});
        checkHotkeyConflicts();
      } else {
        renderConflictAnalysis(buildLocalDuplicateResults(collectCurrentBindings()));
      }
    });
  }

  if (toggleRadarEnabled) {
    toggleRadarEnabled.addEventListener('change', async () => {
      const want = Boolean(toggleRadarEnabled.checked);
      showFeedback('');
      const res = await apiRequest('/set_radar', 'POST', {
        enabled: want,
        radar_enabled: want
      });
      if (res && res.ok && res.data) {
        const nextVal = typeof res.data.radar_enabled === 'boolean'
          ? res.data.radar_enabled
          : (typeof res.data.enabled === 'boolean' ? res.data.enabled : want);
        toggleRadarEnabled.checked = nextVal;
      } else {
        toggleRadarEnabled.checked = !want;
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (toggleCompactUi) {
    toggleCompactUi.addEventListener('change', async () => {
      const want = Boolean(toggleCompactUi.checked);
      await writeLocalStorage({ gh_compact_ui: want });
      if (cachedTabCount === 0) {
        const re = await sendBg({ kind: 'RECONNECT_MAP_TABS' });
        if (re && typeof re.tabCount === 'number') {
          cachedTabCount = re.tabCount;
          if (mapTabStatus) {
            mapTabStatus.textContent = t('popup.footer.map_tabs', { count: cachedTabCount });
          }
        }
      }
      notifyMapTabs({ kind: 'GH_SET_COMPACT_UI', enabled: want });
    });
  }

  for (const btn of tabButtons) {
    btn.addEventListener('click', () => {
      const targetTab = btn.getAttribute('data-tab');
      if (targetTab) switchTab(targetTab);
    });
    btn.addEventListener('keydown', (e) => {
      if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
      e.preventDefault();
      const idx = tabButtons.indexOf(btn);
      if (idx < 0) return;
      let nextIdx = idx;
      if (e.key === 'ArrowRight') nextIdx = (idx + 1) % tabButtons.length;
      else if (e.key === 'ArrowLeft') nextIdx = (idx - 1 + tabButtons.length) % tabButtons.length;
      else if (e.key === 'Home') nextIdx = 0;
      else if (e.key === 'End') nextIdx = tabButtons.length - 1;
      const nextBtn = tabButtons[nextIdx];
      if (nextBtn) {
        const nextTab = nextBtn.getAttribute('data-tab');
        if (nextTab) switchTab(nextTab);
        nextBtn.focus();
      }
    });
  }

  function setPowerBusyUI(activeBtn, busyLabel) {
    powerBusy = true;
    if (btnPowerToggle) btnPowerToggle.disabled = true;
    if (btnPowerStandby) btnPowerStandby.disabled = true;
    if (btnHardQuit) btnHardQuit.disabled = true;
    if (activeBtn) {
      activeBtn.classList.add('is-busy');
      setBtnLabel(activeBtn, busyLabel);
    }
  }

  function clearPowerBusyUI(activeBtn) {
    powerBusy = false;
    if (activeBtn) activeBtn.classList.remove('is-busy');
    setBtnLabel(btnPowerToggle, t('popup.power.active.label'));
    if (btnPowerStandby) setBtnLabel(btnPowerStandby, t('popup.power.standby.label'));
    if (btnHardQuit) setBtnLabel(btnHardQuit, t('popup.power.quit.label'));
  }

  btnPowerToggle.addEventListener('click', async () => {
    if (powerBusy) return;
    if (currentConnected && currentEngineActive) {
      await refreshFromBg();
      return;
    }
    setPowerBusyUI(btnPowerToggle, t('popup.power.starting'));
    nativeHostBanner.hidden = true;
    showFeedback('');

    const res = await sendBg({ kind: 'SET_ENGINE_POWER', active: true });
    clearPowerBusyUI(btnPowerToggle);

    if (res && res.ok) {
      await refreshFromBg();
      const statusRes = await apiRequest('/status', 'GET');
      if (statusRes && statusRes.ok && statusRes.data) {
        applyState({
          connected: true,
          engineActive: typeof statusRes.data.engine_active === 'boolean' ? statusRes.data.engine_active : true,
          statusData: statusRes.data,
          lastSync: Object.prototype.hasOwnProperty.call(statusRes.data, 'last_sync') ? statusRes.data.last_sync : currentLastSync
        });
      }
      showFeedback(
        res.via === 'native_host'
          ? t('popup.power.feedback.started_native')
          : t('popup.power.feedback.resumed'),
        'success',
        2600
      );
    } else if (res && res.needNativeHost) {
      applyState({ connected: false, engineActive: false });
      nativeHostBanner.hidden = false;
    } else {
      await refreshFromBg();
      showFeedback((res && res.error) || t('popup.power.feedback.start_failed'), 'error');
    }
  });

  if (btnPowerStandby) {
    btnPowerStandby.addEventListener('click', async () => {
      if (powerBusy || !currentConnected || !currentEngineActive) return;
      setPowerBusyUI(btnPowerStandby, t('popup.power.stopping'));
      nativeHostBanner.hidden = true;
      showFeedback('');

      const res = await sendBg({ kind: 'SET_ENGINE_POWER', active: false });
      clearPowerBusyUI(btnPowerStandby);

      if (res && res.alreadyOffline) {
        applyState({ connected: false, engineActive: false });
        showFeedback(t('popup.power.feedback.already_offline'), 'warn', 2200);
      } else {
        await refreshFromBg();
        showFeedback(t('popup.power.feedback.standby'), 'success', 2400);
      }
    });
  }

  if (btnHardQuit) {
    btnHardQuit.addEventListener('click', async () => {
      if (powerBusy || !currentConnected) return;
      setPowerBusyUI(btnHardQuit, t('popup.quit.stopping'));
      nativeHostBanner.hidden = true;
      showFeedback('');

      const res = await sendBg({ kind: 'HARD_SHUTDOWN' });
      clearPowerBusyUI(btnHardQuit);

      applyState({ connected: false, engineActive: false, statusData: null });
      if (res && res.ok) {
        showFeedback(t('popup.quit.done'), 'warn', 3000);
      } else {
        showFeedback(t('popup.quit.already_off'), 'warn', 2500);
      }
    });
  }

  toggleAutoSync.addEventListener('change', async () => {
    const want = Boolean(toggleAutoSync.checked);
    showFeedback('');
    const res = await apiRequest('/set_auto_sync', 'POST', { auto_sync_m: want });
    if (res && res.ok && res.data) {
      toggleAutoSync.checked = Boolean(res.data.auto_sync_m);
    } else {
      toggleAutoSync.checked = !want;
      showFeedback(t('popup.modes.offline_error'), 'error');
    }
  });

  if (toggleMinimapTracking) {
    toggleMinimapTracking.addEventListener('change', async () => {
      const want = Boolean(toggleMinimapTracking.checked);
      showFeedback('');
      const res = await apiRequest('/set_minimap_tracking', 'POST', {
        enabled: want,
        minimap_tracking: want
      });
      if (res && res.ok && res.data) {
        const nextVal = typeof res.data.minimap_tracking === 'boolean'
          ? res.data.minimap_tracking
          : Boolean(res.data.enabled);
        toggleMinimapTracking.checked = nextVal;
      } else {
        toggleMinimapTracking.checked = !want;
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (toggleNavHud) {
    toggleNavHud.addEventListener('change', async () => {
      const want = Boolean(toggleNavHud.checked);
      syncHudSubGroupState();
      showFeedback('');
      const res = await apiRequest('/set_nav_hud', 'POST', {
        enabled: want,
        nav_hud_enabled: want
      });
      if (res && res.ok && res.data) {
        const nextVal = typeof res.data.nav_hud_enabled === 'boolean'
          ? res.data.nav_hud_enabled
          : Boolean(res.data.enabled);
        toggleNavHud.checked = nextVal;
      } else {
        toggleNavHud.checked = !want;
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
      syncHudSubGroupState();
    });
  }

  if (selectHudPosition) {
    selectHudPosition.addEventListener('change', async () => {
      const want = selectHudPosition.value;
      showFeedback('');
      const res = await apiRequest('/set_nav_hud', 'POST', { position: want, nav_hud_position: want });
      if (res && res.ok && res.data) {
        if (res.data.edit_mode === true) {
          showFeedback(t('popup.nav_hud.edit_started'), 'success', 4200);
        } else if (typeof res.data.position === 'string') {
          selectHudPosition.value = res.data.position;
        }
      } else {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (btnHudEdit) {
    btnHudEdit.addEventListener('click', async () => {
      showFeedback('');
      const res = await apiRequest('/set_nav_hud', 'POST', { edit_mode: 'toggle' });
      if (res && res.ok && res.data) {
        const editing = res.data.edit_mode === true;
        btnHudEdit.classList.toggle('is-active', editing);
        showFeedback(t(editing ? 'popup.nav_hud.edit_started' : 'popup.nav_hud.edit_saved'), 'success', editing ? 4200 : 2200);
      } else {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (toggleHudCapture) {
    toggleHudCapture.addEventListener('change', async () => {
      const want = Boolean(toggleHudCapture.checked);
      showFeedback('');
      const res = await apiRequest('/set_nav_hud', 'POST', {
        capture_visible: want,
        nav_hud_capture_visible: want
      });
      if (res && res.ok && res.data) {
        const nextVal = typeof res.data.nav_hud_capture_visible === 'boolean'
          ? res.data.nav_hud_capture_visible
          : (typeof res.data.capture_visible === 'boolean' ? res.data.capture_visible : want);
        toggleHudCapture.checked = nextVal;
      } else {
        toggleHudCapture.checked = !want;
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (selectHudGuideZoom) {
    selectHudGuideZoom.addEventListener('change', async () => {
      const want = selectHudGuideZoom.value;
      showFeedback('');
      const res = await apiRequest('/set_nav_hud', 'POST', {
        guide_zoom_mode: want,
        nav_hud_guide_zoom_mode: want
      });
      if (res && res.ok && res.data) {
        const nextMode = res.data.nav_hud_guide_zoom_mode || res.data.guide_zoom_mode || want;
        if (Array.from(selectHudGuideZoom.options).some((o) => o.value === nextMode)) {
          selectHudGuideZoom.value = nextMode;
        }
      } else {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (toggleAudioEnabled) {
    toggleAudioEnabled.addEventListener('change', async () => {
      const want = Boolean(toggleAudioEnabled.checked);
      showFeedback('');
      if (audioVolRow) {
        audioVolRow.classList.toggle('is-muted', !want);
      }
      const res = await apiRequest('/set_audio', 'POST', {
        audio_enabled: want,
        enabled: want
      });
      if (res && res.ok && res.data) {
        const nextVal = typeof res.data.audio_enabled === 'boolean' ? res.data.audio_enabled : want;
        toggleAudioEnabled.checked = nextVal;
        if (audioVolRow) {
          audioVolRow.classList.toggle('is-muted', !nextVal);
        }
      } else {
        toggleAudioEnabled.checked = !want;
        if (audioVolRow) {
          audioVolRow.classList.toggle('is-muted', want);
        }
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (rangeAudioVolume) {
    rangeAudioVolume.addEventListener('input', () => {
      if (audioVolumeValue) {
        audioVolumeValue.textContent = `${rangeAudioVolume.value}%`;
      }
    });
    rangeAudioVolume.addEventListener('change', async () => {
      const wantVol = Math.max(0, Math.min(100, Math.round(Number(rangeAudioVolume.value) || 0)));
      showFeedback('');
      const res = await apiRequest('/set_audio', 'POST', {
        audio_volume: wantVol,
        volume: wantVol
      });
      if (res && res.ok && res.data && typeof res.data.audio_volume === 'number') {
        const actual = Math.max(0, Math.min(100, Math.round(res.data.audio_volume)));
        rangeAudioVolume.value = String(actual);
        if (audioVolumeValue) {
          audioVolumeValue.textContent = `${actual}%`;
        }
      } else if (!res || !res.ok) {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (btnAudioTest) {
    btnAudioTest.addEventListener('click', async () => {
      btnAudioTest.disabled = true;
      showFeedback('');
      const res = await apiRequest('/set_audio', 'POST', { test: true });
      btnAudioTest.disabled = false;
      if (!res || !res.ok) {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  function renderRouteCardStatus(statusObj) {
    if (statusObj && typeof statusObj === 'object') {
      cachedRouteStatus = statusObj;
    }
    const active = Boolean(cachedRouteStatus && cachedRouteStatus.active);
    if (routeStatusText) {
      if (active) {
        const title = String(cachedRouteStatus.title || 'Route');
        const sec = Number(cachedRouteStatus.active_section || 0) + 1;
        const secs = Math.max(1, Number(cachedRouteStatus.section_count || 1));
        const done = Number(cachedRouteStatus.section_done || 0);
        const total = Number(cachedRouteStatus.section_total || 0);
        routeStatusText.textContent = t('popup.route.active_fmt', { title, sec, secs, done, total });
      } else {
        routeStatusText.textContent = t('popup.route.idle');
      }
    }
    if (btnRouteExport) btnRouteExport.disabled = !active;
    if (btnRouteClear) btnRouteClear.disabled = !active;
  }

  async function refreshRouteStatus() {
    if (!currentConnected) {
      renderRouteCardStatus({ active: false });
      return;
    }
    if (routeStatusInFlight) return;
    routeStatusInFlight = true;
    try {
      const res = await apiRequest('/route', 'GET');
      if (res && res.ok && res.data && typeof res.data === 'object') {
        renderRouteCardStatus(res.data);
      }
    } finally {
      routeStatusInFlight = false;
    }
  }

  function triggerPopupDownload(filename, contentStr) {
    const blob = new Blob([contentStr], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename || 'route.ghroute.json';
    (document.body || document.documentElement).appendChild(a);
    a.click();
    setTimeout(() => {
      try { a.remove(); } catch (e) {}
      try { URL.revokeObjectURL(url); } catch (e) {}
    }, 800);
  }

  if (selectMmapOverlayMode) {
    selectMmapOverlayMode.addEventListener('change', async () => {
      const want = selectMmapOverlayMode.value;
      showFeedback('');
      const res = await apiRequest('/set_nav_hud', 'POST', { mmap_overlay_mode: want });
      if (res && res.ok && res.data && typeof res.data.mmap_overlay_mode === 'string') {
        selectMmapOverlayMode.value = res.data.mmap_overlay_mode;
      } else {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (btnRouteExport) {
    btnRouteExport.addEventListener('click', async () => {
      btnRouteExport.disabled = true;
      showFeedback('');
      const res = await apiRequest('/route', 'POST', { action: 'export' });
      btnRouteExport.disabled = false;
      if (res && res.ok && res.data && res.data.doc) {
        const fname = String(res.data.filename || 'route.ghroute.json');
        triggerPopupDownload(fname, JSON.stringify(res.data.doc, null, 2));
        showFeedback(t('popup.route.exported', { file: fname }), 'success', 2600);
      } else {
        showFeedback(t('popup.route.none_active'), 'warn', 2600);
      }
      renderRouteCardStatus(cachedRouteStatus);
    });
  }

  if (btnRouteImport && routeImportTray) {
    btnRouteImport.addEventListener('click', () => {
      routeImportTray.hidden = !routeImportTray.hidden;
    });
  }

  if (btnRoutePickFile && inputRouteFile) {
    btnRoutePickFile.addEventListener('click', () => {
      inputRouteFile.value = '';
      inputRouteFile.click();
    });
    inputRouteFile.addEventListener('change', async () => {
      const file = inputRouteFile.files && inputRouteFile.files[0];
      if (!file) return;
      showFeedback('');
      try {
        const rawText = await file.text();
        const parsed = JSON.parse(rawText);
        const res = await apiRequest('/route', 'POST', { action: 'import', raw: parsed });
        if (res && res.ok && res.data && res.data.active) {
          renderRouteCardStatus(res.data);
          if (routeImportTray) routeImportTray.hidden = true;
          await sendBg({ kind: 'GH_CMD_ROUTE_IMPORT', payload: { fit: true } }).catch(() => {});
          showFeedback(t('popup.route.imported', { title: String(res.data.title || file.name) }), 'success', 2600);
        } else {
          const err = (res && res.data && res.data.error) || (res && res.error) || 'Import failed';
          showFeedback(t('popup.route.invalid_file', { error: err }), 'error');
        }
      } catch (err) {
        const msg = (err && err.message) ? err.message : 'Invalid JSON';
        showFeedback(t('popup.route.invalid_file', { error: msg }), 'error');
      }
    });
  }

  if (btnRouteOpenUrl && inputRouteUrl) {
    btnRouteOpenUrl.addEventListener('click', async () => {
      const rawUrl = (inputRouteUrl.value || '').trim();
      if (!rawUrl.startsWith('https://act.hoyolab.com/ys/app/interactive-map/')) {
        showFeedback(t('popup.route.invalid_url'), 'warn', 2800);
        return;
      }
      btnRouteOpenUrl.disabled = true;
      showFeedback('');
      const res = await sendBg({ kind: 'GH_CMD_OPEN_ROUTE_URL', url: rawUrl });
      btnRouteOpenUrl.disabled = false;
      if (res && res.ok) {
        if (routeImportTray) routeImportTray.hidden = true;
        showFeedback(t('popup.route.opening_url'), 'success', 2400);
      } else {
        showFeedback((res && res.error) || t('popup.route.invalid_url'), 'error');
      }
    });
  }

  if (btnRouteClear) {
    btnRouteClear.addEventListener('click', async () => {
      btnRouteClear.disabled = true;
      showFeedback('');
      const res = await apiRequest('/route', 'POST', { action: 'clear' });
      btnRouteClear.disabled = false;
      if (res && res.ok) {
        renderRouteCardStatus({ active: false });
        await sendBg({ kind: 'GH_CMD_ROUTE_CLEAR' }).catch(() => {});
        showFeedback(t('popup.route.cleared'), 'success', 2000);
      } else {
        showFeedback(t('popup.modes.offline_error'), 'error');
      }
    });
  }

  if (selectExportFormat) {
    selectExportFormat.addEventListener('change', () => {
      writeLocalStorage({ gh_export_format: selectExportFormat.value });
    });
  }

  if (btnExportPins) {
    btnExportPins.addEventListener('click', async () => {
      const fmt = selectExportFormat ? selectExportFormat.value : 'canonical';
      btnExportPins.disabled = true;
      showFeedback('');
      const res = await sendBg({ kind: 'GH_CMD_EXPORT_PINS', targetFormat: fmt });
      btnExportPins.disabled = false;
      if (res && res.ok) {
        showFeedback(t('popup.export.started', { format: fmt }), 'success', 2800);
      } else if (res && res.noTab) {
        showFeedback(t('popup.tools.need_map_tab'), 'warn', 3200);
      } else {
        showFeedback((res && res.error) || t('popup.tools.need_map_tab'), 'error');
      }
    });
  }

  if (btnToggleLan) {
    btnToggleLan.addEventListener('click', async () => {
      if (lanBusy) return;
      const wantStart = !lanRunning;
      lanBusy = true;
      btnToggleLan.disabled = true;
      setBtnLabel(btnToggleLan, wantStart ? t('popup.lan.starting') : t('popup.lan.stopping'));
      showFeedback('');

      const res = await apiRequest('/lan_companion', 'POST', { action: wantStart ? 'start' : 'stop' });
      lanBusy = false;
      btnToggleLan.disabled = false;

      if (res && res.ok && res.data && res.data.ok !== false) {
        const isOn = Boolean(res.data.running || res.data.is_running);
        lanRunning = isOn;
        lanInfo = isOn ? res.data : null;
        renderLanCompanionUI();
        showFeedback(isOn ? t('popup.lan.started') : t('popup.lan.stopped'), isOn ? 'success' : 'warn', 2600);
      } else {
        renderLanCompanionUI();
        const errText = (res && res.data && res.data.error) || (res && res.error);
        if (!currentConnected) {
          showFeedback(t('popup.modes.offline_error'), 'error');
        } else {
          showFeedback(t('popup.lan.error', { error: errText || 'Unknown error' }), 'error');
        }
      }
    });
  }

  if (btnCopyLanUrl) {
    btnCopyLanUrl.addEventListener('click', async () => {
      const urlStr = (lanInfo && lanInfo.url) ? String(lanInfo.url) : (lanUrlText ? lanUrlText.textContent : '');
      if (!urlStr) return;
      try {
        if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
          await navigator.clipboard.writeText(urlStr);
        }
        showFeedback(t('popup.lan.copied'), 'success', 1800);
      } catch (e) {}
    });
  }

  if (btnLaunchBorderless) {
    btnLaunchBorderless.addEventListener('click', async () => {
      btnLaunchBorderless.disabled = true;
      setBtnLabel(btnLaunchBorderless, t('popup.borderless.launching'));
      showFeedback('');
      const res = await apiRequest('/launch_borderless', 'POST', {});
      btnLaunchBorderless.disabled = false;
      setBtnLabel(btnLaunchBorderless, t('popup.borderless.btn'));

      if (res && res.ok && res.data && (res.data.ok || res.data.launched)) {
        showFeedback(t('popup.borderless.launched'), 'success', 3000);
      } else {
        const errMsg = (res && res.data && (res.data.message || res.data.error)) || (res && res.error) || t('popup.modes.offline_error');
        showFeedback(t('popup.borderless.failed', { error: errMsg }), 'error');
      }
    });
  }

  if (selectMapProvider) {
    selectMapProvider.addEventListener('change', () => {
      const val = String(selectMapProvider.value || 'hoyolab');
      writeLocalStorage({ gh_preferred_provider: val });
    });
  }

  btnOpenMap.addEventListener('click', async () => {
    btnOpenMap.disabled = true;
    const providerId = selectMapProvider ? String(selectMapProvider.value || 'hoyolab') : 'hoyolab';
    await sendBg({ kind: 'OPEN_MAP_TAB', providerId });
    window.close();
  });

  btnSwitchGame.addEventListener('click', async () => {
    btnSwitchGame.disabled = true;
    showFeedback('');
    const res = await apiRequest('/quick_switch', 'POST', {});
    btnSwitchGame.disabled = false;

    if (!res || !res.ok || !res.data) {
      showFeedback(t('popup.action.switch_offline'), 'error');
      return;
    }
    const target = res.data.target;
    if (target === 'game' || target === 'browser') {
      window.close();
    } else if (target === 'no_browser') {
      showFeedback(t('popup.action.switch_no_browser'));
    } else {
      showFeedback(t('popup.action.switch_no_game'));
    }
  });

  btnSaveHotkeys.addEventListener('click', async () => {
    if (activeRecordTarget || activeExtRecord) {
      stopRecording(null, null, false);
    }
    const payload = {};
    for (const act of HOTKEY_ACTIONS) {
      const rec = recorders[act];
      if (!rec) continue;
      payload[`hotkey_${act}`] = (rec.val || '').trim().toLowerCase();
    }

    if (Object.values(payload).some((v) => !v)) {
      showFeedback(t('popup.hotkeys.missing_error'), 'error');
      return;
    }
    const seenKeys = new Map();
    for (const [k, v] of Object.entries(payload)) {
      if (seenKeys.has(v)) {
        showFeedback(t('popup.hotkeys.duplicate_error', { key: v.toUpperCase() }), 'error');
        return;
      }
      seenKeys.set(v, k);
    }

    btnSaveHotkeys.disabled = true;
    setBtnLabel(btnSaveHotkeys, t('popup.hotkeys.saving'));
    showFeedback('');

    const res = await apiRequest('/set_hotkeys', 'POST', payload);

    btnSaveHotkeys.disabled = false;
    setBtnLabel(btnSaveHotkeys, t('popup.hotkeys.save_btn'));

    if (!res || !res.ok || !res.data || !res.data.results) {
      showFeedback(t('popup.hotkeys.offline_error'), 'error');
      return;
    }

    const results = res.data.results;
    const failed = [];
    for (const act of HOTKEY_ACTIONS) {
      const r = results[act];
      if (r && r.ok === false) {
        failed.push(act);
      } else if (r && r.ok) {
        const rec = recorders[act];
        if (rec) {
          rec.val = r.hotkey || rec.val;
          rec.displayText = r.display_name || rec.displayText;
          rec.display.textContent = rec.displayText;
          rec.box.classList.remove('is-dirty');
          rec.badge.textContent = t('popup.hotkeys.badge_change');
        }
      }
    }

    if (failed.length > 0) {
      showFeedback(t('popup.hotkeys.register_failed', { actions: failed.join(', ') }), 'error');
      return;
    }

    hotkeysDirty = false;
    showFeedback(t('popup.hotkeys.saved'), 'success', 2600);
    checkHotkeyConflicts();
  });

  function formatBytesHuman(bytes) {
    const b = Number(bytes) || 0;
    if (b <= 0) return '0 KB';
    if (b < 1024 * 1024) return `${Math.max(1, Math.round(b / 1024))} KB`;
    return `${(b / (1024 * 1024)).toFixed(2)} MB`;
  }

  function renderDiagResultMeta(bundle) {
    if (!diagZipMeta || !bundle) return;
    const sizeText = formatBytesHuman(bundle.zip_size_bytes);
    const hasBrowser = Boolean(bundle.summary && bundle.summary.browser_captured);
    diagZipMeta.textContent = hasBrowser
      ? t('popup.diag.saved_meta_browser', { size: sizeText })
      : t('popup.diag.saved_meta_nobrowser', { size: sizeText });
  }

  function getSelectedDiagCategoryLabel() {
    if (!selectedDiagCategory) return '';
    const chip = diagCatChips.find((c) => c.getAttribute('data-diag-cat') === selectedDiagCategory);
    return chip ? chip.textContent.trim() : selectedDiagCategory;
  }

  function formatDiscordReportTemplate(bundle) {
    const zipName = (bundle && (bundle.zip_name || (bundle.folder_name ? `${bundle.folder_name}.zip` : ''))) || 'diag_bundle.zip';
    const summary = (bundle && bundle.summary && typeof bundle.summary === 'object') ? bundle.summary : {};
    const appVer = summary.app_version || '5.7.0';
    let extVer = '5.7.0';
    try {
      extVer = (chrome.runtime && chrome.runtime.getManifest && chrome.runtime.getManifest().version) || '5.7.0';
    } catch (e) {}
    const catLabel = getSelectedDiagCategoryLabel() || t('popup.diag.cat_other');
    const noteText = (bundle && bundle.user_note) || (diagNoteInput ? diagNoteInput.value.trim() : '') || '-';
    const osStr = summary.os_label || 'Windows';
    const resStr = summary.resolution_label || 'N/A';
    const dpiStr = summary.dpi_scale_pct ? `${summary.dpi_scale_pct}%` : 'N/A';
    const flags = [];
    if (summary.game_found === false) flags.push('no-game-window');
    if (summary.hdr_clipped) flags.push('HDR-clipped');
    if (summary.uipi_blocked) flags.push('UIPI-admin-game');
    if (summary.browser_captured) flags.push('map-viewport-ok');
    const envLine = `${osStr} | ${resStr} @ ${dpiStr}${flags.length ? ` (${flags.join(', ')})` : ''}`;
    return [
      `**[${catLabel}]** \`${zipName}\``,
      `- **App / Ext:** \`v${appVer}\` / \`v${extVer}\``,
      `- **Env:** ${envLine}`,
      `- **Details:** ${noteText}`,
      `_${t('popup.diag.discord_footer', { zip: zipName })}_`
    ].join('\n');
  }

  async function copyPlainText(text) {
    if (!text) return false;
    try {
      if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        await navigator.clipboard.writeText(text);
        return true;
      }
    } catch (e) {}
    try {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand('copy');
      ta.remove();
      return Boolean(ok);
    } catch (e) {
      return false;
    }
  }

  function stopUpdatePolling() {
    if (updatePollTimer) {
      clearInterval(updatePollTimer);
      updatePollTimer = null;
    }
  }

  function ensureUpdatePollingIfActive() {
    const isOpen = Boolean(updateDrawer && !updateDrawer.hidden);
    const phase = (currentUpdateState && currentUpdateState.phase) || '';
    const mapPhase = (currentUpdateState && currentUpdateState.map_data && currentUpdateState.map_data.phase) || '';
    const active = isOpen && (
      phase === 'checking' ||
      phase === 'downloading' ||
      phase === 'verifying' ||
      phase === 'installing' ||
      mapPhase === 'checking' ||
      mapPhase === 'updating'
    );
    if (!active) {
      stopUpdatePolling();
      return;
    }
    if (!updatePollTimer) {
      updatePollTimer = setInterval(() => {
        if (currentConnected) {
          fetchAppUpdateStatus(true);
        } else {
          refreshFromBg();
        }
      }, 650);
    }
  }

  async function fetchAppUpdateStatus(quiet = false) {
    if (!currentConnected) {
      if (currentUpdateState && currentUpdateState.phase === 'installing') {
        renderUpdateDrawerUI();
        return null;
      }
      renderUpdateDrawerUI();
      return null;
    }
    const res = await apiRequest('/app_update', 'GET');
    if (res && res.ok && res.data && typeof res.data === 'object') {
      currentUpdateState = res.data;
      renderUpdateDrawerUI();
      ensureUpdatePollingIfActive();
      return res.data;
    }
    if (!quiet) {
      renderUpdateDrawerUI();
    }
    return null;
  }

  function renderUpdateDrawerUI() {
    if (!updateDrawer) return;
    const st = currentUpdateState || {};
    const summary = (currentLastStatus && currentLastStatus.app_update && typeof currentLastStatus.app_update === 'object')
      ? currentLastStatus.app_update
      : {};
    const curVer = String(
      st.current_version ||
      (currentLastStatus && currentLastStatus.version) ||
      '5.7.0'
    ).replace(/^v/i, '');
    const latestVer = String(st.latest_version || summary.latest_version || '').replace(/^v/i, '');
    const phase = String(st.phase || summary.phase || 'idle');
    const hasUpdate = Boolean(st.has_update ?? summary.has_update) && Boolean(latestVer);
    const canOneClick = Boolean(st.can_one_click ?? summary.can_one_click);
    const checksumPresent = Boolean(st.checksum_present ?? canOneClick);
    const verifiedReady = Boolean(st.verified_ready ?? summary.verified_ready);
    const willInstallOnExit = Boolean(st.will_install_on_exit ?? summary.will_install_on_exit);
    const autoInstallBlocked = Boolean(st.auto_install_blocked ?? summary.auto_install_blocked);
    const justUpdatedFrom = String(st.just_updated_from || summary.just_updated_from || '').replace(/^v/i, '');
    const downloadedBytes = Number(st.downloaded_bytes) || 0;
    const totalBytes = Number(st.total_bytes || st.installer_size) || 0;
    const pct = typeof st.progress_pct === 'number'
      ? Math.max(0, Math.min(100, Math.round(st.progress_pct)))
      : (totalBytes > 0 ? Math.min(100, Math.round((downloadedBytes * 100) / totalBytes)) : 0);

    if (updateVerCurrent) updateVerCurrent.textContent = `v${curVer}`;
    if (updateVerArrow) updateVerArrow.hidden = !hasUpdate;
    if (updateVerLatest) {
      updateVerLatest.hidden = !hasUpdate;
      if (hasUpdate) updateVerLatest.textContent = `v${latestVer}`;
    }

    if (updateShaPill) {
      if (hasUpdate) {
        updateShaPill.hidden = false;
        updateShaPill.classList.toggle('is-warn', !checksumPresent);
        if (verifiedReady || phase === 'ready_to_install' || phase === 'installing') {
          updateShaPill.textContent = t('popup.update.sha_verified');
        } else if (checksumPresent) {
          updateShaPill.textContent = t('popup.update.sha_ready');
        } else {
          updateShaPill.textContent = t('popup.update.sha_missing');
        }
      } else {
        updateShaPill.hidden = true;
      }
    }

    if (updateStatusText) {
      updateStatusText.classList.remove('is-success', 'is-error');
      if (phase === 'installing') {
        updateStatusText.classList.add('is-success');
        updateStatusText.textContent = t('popup.update.status_installing', { latest: latestVer || curVer });
      } else if (!currentConnected) {
        updateStatusText.textContent = t('popup.update.status_offline');
      } else if (phase === 'checking') {
        updateStatusText.textContent = t('popup.update.status_checking');
      } else if (phase === 'up_to_date') {
        updateStatusText.classList.add('is-success');
        updateStatusText.textContent = t('popup.update.status_up_to_date', { current: curVer });
      } else if (phase === 'downloading') {
        updateStatusText.textContent = t('popup.update.status_downloading', { latest: latestVer || curVer, pct });
      } else if (phase === 'verifying') {
        updateStatusText.textContent = t('popup.update.status_verifying');
      } else if (phase === 'ready_to_install') {
        updateStatusText.classList.add('is-success');
        updateStatusText.textContent = willInstallOnExit
          ? t('popup.update.status_ready_auto_exit', { latest: latestVer || curVer })
          : t('popup.update.status_ready', { latest: latestVer || curVer });
      } else if (phase === 'available' && hasUpdate) {
        if (canOneClick && autoInstallBlocked) {
          updateStatusText.textContent = t('popup.update.status_available_auto_blocked', {
            latest: latestVer,
            size: formatBytesHuman(totalBytes)
          });
        } else if (canOneClick) {
          updateStatusText.classList.add('is-success');
          updateStatusText.textContent = t('popup.update.status_available', {
            latest: latestVer,
            size: formatBytesHuman(totalBytes)
          });
        } else {
          updateStatusText.textContent = t('popup.update.status_available_no_sha', { latest: latestVer });
        }
      } else if (phase === 'error') {
        updateStatusText.classList.add('is-error');
        updateStatusText.textContent = t('popup.update.status_error', { error: st.error || 'Network error' });
      } else if (justUpdatedFrom) {
        updateStatusText.classList.add('is-success');
        updateStatusText.textContent = t('popup.update.status_just_updated', {
          prev: justUpdatedFrom,
          current: curVer
        });
      } else {
        updateStatusText.textContent = t('popup.update.status_idle');
      }
    }

    const showProgress = currentConnected && (phase === 'downloading' || phase === 'verifying');
    if (updateProgressWrap) {
      updateProgressWrap.hidden = !showProgress;
      if (showProgress) {
        if (updateProgressFill) updateProgressFill.style.width = `${pct}%`;
        if (updateProgressMeta) {
          updateProgressMeta.textContent = totalBytes > 0
            ? `${pct}% • ${formatBytesHuman(downloadedBytes)} / ${formatBytesHuman(totalBytes)}`
            : `${pct}%`;
        }
      }
    }

    if (updateNotesBox) {
      const notes = typeof st.release_notes === 'string' ? st.release_notes.trim() : '';
      if (hasUpdate && notes) {
        updateNotesBox.hidden = false;
        updateNotesBox.textContent = notes;
      } else {
        updateNotesBox.hidden = true;
        updateNotesBox.textContent = '';
      }
    }

    if (btnUpdatePrimary) {
      btnUpdatePrimary.disabled = !currentConnected || updateActionBusy || phase === 'checking' || phase === 'verifying' || phase === 'installing';
      btnUpdatePrimary.classList.toggle('btn-update-ready', phase === 'ready_to_install' && hasUpdate);
      if (phase === 'checking') {
        btnUpdatePrimary.dataset.state = 'busy';
        setBtnLabel(btnUpdatePrimary, t('popup.update.checking_btn'));
      } else if (phase === 'downloading') {
        btnUpdatePrimary.disabled = !currentConnected;
        btnUpdatePrimary.dataset.state = 'busy';
        setBtnLabel(btnUpdatePrimary, t('popup.update.cancel_btn'));
      } else if (phase === 'installing') {
        btnUpdatePrimary.dataset.state = 'busy';
        setBtnLabel(btnUpdatePrimary, t('popup.update.installing_btn'));
      } else if (phase === 'ready_to_install' && hasUpdate) {
        btnUpdatePrimary.dataset.state = 'ready';
        setBtnLabel(btnUpdatePrimary, t('popup.update.restart_update_btn', { latest: latestVer }));
      } else if (phase === 'available' && hasUpdate && canOneClick) {
        btnUpdatePrimary.dataset.state = 'download';
        setBtnLabel(btnUpdatePrimary, t('popup.update.install_1click_btn', { latest: latestVer }));
      } else {
        btnUpdatePrimary.dataset.state = 'idle';
        setBtnLabel(btnUpdatePrimary, t('popup.update.check_btn'));
      }
    }

    if (selectUpdateMode && document.activeElement !== selectUpdateMode) {
      const resolvedMode = typeof st.app_update_mode === 'string'
        ? st.app_update_mode
        : (typeof summary.app_update_mode === 'string'
            ? summary.app_update_mode
            : (currentLastStatus && typeof currentLastStatus.app_update_mode === 'string'
                ? currentLastStatus.app_update_mode
                : 'auto'));
      if (Array.from(selectUpdateMode.options).some((o) => o.value === resolvedMode)) {
        selectUpdateMode.value = resolvedMode;
      }
      selectUpdateMode.disabled = !currentConnected;
    }

    const mapData = (st.map_data && typeof st.map_data === 'object') ? st.map_data : {};
    const mapPhase = String(mapData.phase || summary.map_data_phase || 'idle');
    const mapChangedCount = Math.max(
      1,
      (Array.isArray(mapData.updated_maps) ? mapData.updated_maps.length : 0) +
      (Array.isArray(mapData.new_map_ids) ? mapData.new_map_ids.length : 0)
    );
    if (updateMapStatus) {
      updateMapStatus.classList.remove('is-success', 'is-error');
      if (mapPhase === 'checking' || mapPhase === 'updating') {
        updateMapStatus.textContent = t('popup.update.map_checking');
      } else if (mapPhase === 'up_to_date') {
        updateMapStatus.classList.add('is-success');
        updateMapStatus.textContent = t('popup.update.map_up_to_date');
      } else if (mapPhase === 'update_available') {
        updateMapStatus.textContent = t('popup.update.map_available', { count: mapChangedCount });
      } else if (mapPhase === 'updated') {
        updateMapStatus.classList.add('is-success');
        updateMapStatus.textContent = t('popup.update.map_updated', { count: mapChangedCount });
      } else if (mapPhase === 'disabled') {
        updateMapStatus.textContent = t('popup.update.map_disabled');
      } else if (mapPhase === 'error') {
        updateMapStatus.classList.add('is-error');
        updateMapStatus.textContent = t('popup.update.map_error', { error: mapData.error || 'Error' });
      } else {
        updateMapStatus.textContent = t('popup.update.map_idle');
      }
    }
    if (btnSyncMapData) {
      const syncing = mapPhase === 'checking' || mapPhase === 'updating';
      btnSyncMapData.disabled = !currentConnected || syncing;
      setBtnLabel(btnSyncMapData, syncing ? t('popup.update.map_syncing_btn') : t('popup.update.map_sync_btn'));
    }
    if (updateUninstallRow) {
      const canSelfUninstall = Boolean(
        typeof chrome !== 'undefined' &&
        chrome.management &&
        typeof chrome.management.uninstallSelf === 'function'
      );
      updateUninstallRow.hidden = currentConnected || !canSelfUninstall;
    }
  }

  const setUpdateOpen = (open) => {
    if (!updateDrawer) return;
    if (open) {
      setDonateOpen(false);
      setDiagOpen(false);
    }
    updateDrawer.hidden = !open;
    if (versionBadge) {
      versionBadge.classList.toggle('is-open', open);
      versionBadge.setAttribute('aria-expanded', open ? 'true' : 'false');
    }
    if (open) {
      renderUpdateDrawerUI();
      fetchAppUpdateStatus(true);
    } else {
      stopUpdatePolling();
    }
  };

  const setDonateOpen = (open) => {
    if (!donateDrawer || !btnDonateToggle) return;
    if (open) {
      setDiagOpen(false);
      setUpdateOpen(false);
    }
    donateDrawer.hidden = !open;
    btnDonateToggle.classList.toggle('is-open', open);
    btnDonateToggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    if (!open && donateQrWrap) {
      donateQrWrap.hidden = true;
      if (btnDonateMomo) btnDonateMomo.setAttribute('aria-expanded', 'false');
      if (donateMomoSub) donateMomoSub.textContent = t('popup.donate.momo_sub');
    }
  };

  const setDiagOpen = (open) => {
    if (!diagDrawer) return;
    if (open) {
      setDonateOpen(false);
      setUpdateOpen(false);
    }
    diagDrawer.hidden = !open;
    if (btnDiagToggle) {
      btnDiagToggle.classList.toggle('is-open', open);
      btnDiagToggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    }
  };

  for (const chip of diagCatChips) {
    chip.addEventListener('click', () => {
      const cat = chip.getAttribute('data-diag-cat') || '';
      selectedDiagCategory = selectedDiagCategory === cat ? '' : cat;
      for (const item of diagCatChips) {
        const active = (item.getAttribute('data-diag-cat') || '') === selectedDiagCategory && selectedDiagCategory !== '';
        item.classList.toggle('is-active', active);
        item.setAttribute('aria-pressed', active ? 'true' : 'false');
      }
    });
  }

  if (btnDiagToggle && diagDrawer) {
    btnDiagToggle.addEventListener('click', () => {
      const nextOpen = diagDrawer.hidden;
      setDiagOpen(nextOpen);
      if (nextOpen && diagNoteInput) setTimeout(() => diagNoteInput.focus(), 20);
    });
  }

  if (btnDiagClose && diagDrawer) {
    btnDiagClose.addEventListener('click', () => {
      setDiagOpen(false);
      if (btnDiagToggle) btnDiagToggle.focus();
    });
  }

  if (btnTriggerDiag) {
    btnTriggerDiag.addEventListener('click', async () => {
      if (diagCaptureBusy) return;
      diagCaptureBusy = true;
      btnTriggerDiag.disabled = true;
      setBtnLabel(btnTriggerDiag, t('popup.diag.capturing_btn'));
      diagHasCustomStatus = true;
      diagStatus.classList.remove('is-success', 'is-error');
      diagStatus.textContent = t('popup.diag.capturing_status');

      const res = await sendBg({
        kind: 'TRIGGER_DIAGNOSTIC_FLOW',
        note: diagNoteInput ? diagNoteInput.value.trim() : '',
        category: selectedDiagCategory,
        includeBrowserCapture: toggleDiagBrowser ? Boolean(toggleDiagBrowser.checked) : true,
        compactUiEnabled: toggleCompactUi ? Boolean(toggleCompactUi.checked) : false,
        lang: getActiveLang()
      });

      diagCaptureBusy = false;
      btnTriggerDiag.disabled = false;
      setBtnLabel(btnTriggerDiag, t('popup.diag.btn'));

      if (res && res.ok && res.data && res.data.bundle) {
        const bundle = res.data.bundle;
        lastDiagBundle = bundle;
        const zipName = bundle.zip_name || (bundle.folder_name ? `${bundle.folder_name}.zip` : 'diag_bundle.zip');
        if (diagResultBox && diagZipName) {
          diagZipName.textContent = zipName;
          renderDiagResultMeta(bundle);
          diagResultBox.hidden = false;
        }
        const md = formatDiscordReportTemplate(bundle);
        const copied = await copyPlainText(md);
        const openRes = await apiRequest(
          '/open_diagnostics',
          'POST',
          bundle.folder_name ? { folder_name: bundle.folder_name } : {}
        );
        const opened = Boolean(openRes && openRes.ok && openRes.data && openRes.data.opened);
        diagStatus.classList.add('is-success');
        if (opened && copied) {
          diagStatus.textContent = t('popup.diag.saved_auto_ready', { folder: zipName });
          showFeedback(t('popup.diag.saved_auto_ready', { folder: zipName }), 'success', 4000);
        } else {
          diagStatus.textContent = t('popup.diag.saved_status', { folder: zipName });
          showFeedback(t('popup.diag.saved_feedback', { folder: zipName }), 'success', 3500);
        }
      } else {
        diagStatus.classList.add('is-error');
        diagStatus.textContent = (res && res.status === 0)
          ? t('popup.diag.offline_error')
          : t('popup.diag.error_status');
      }
    });
  }

  if (btnDiagOpenFolder) {
    btnDiagOpenFolder.addEventListener('click', async () => {
      const folderName = (lastDiagBundle && lastDiagBundle.folder_name) || '';
      const res = await apiRequest('/open_diagnostics', 'POST', folderName ? { folder_name: folderName } : {});
      if (res && res.ok && res.data && res.data.opened) {
        showFeedback(t('popup.diag.folder_opened'), 'success', 2400);
      } else {
        showFeedback(t('popup.diag.folder_open_err'), 'error', 3000);
      }
    });
  }

  if (btnDiagCopyTemplate) {
    btnDiagCopyTemplate.addEventListener('click', async () => {
      const md = formatDiscordReportTemplate(lastDiagBundle);
      const copied = await copyPlainText(md);
      if (copied) {
        showFeedback(t('popup.diag.copied_discord'), 'success', 2600);
      }
    });
  }

  if (btnDonateToggle && donateDrawer) {
    btnDonateToggle.addEventListener('click', () => {
      setDonateOpen(donateDrawer.hidden);
    });
    if (btnDonateClose) {
      btnDonateClose.addEventListener('click', () => {
        setDonateOpen(false);
        btnDonateToggle.focus();
      });
    }
    if (donateQrImg && btnDonateMomo) {
      donateQrImg.addEventListener('error', () => {
        btnDonateMomo.hidden = true;
        if (donateQrWrap) donateQrWrap.hidden = true;
        const grid = btnDonateMomo.closest('.donate-grid');
        if (grid) grid.classList.add('is-single');
      });
    }
    if (btnDonateMomo && donateQrWrap) {
      btnDonateMomo.addEventListener('click', () => {
        const showQr = donateQrWrap.hidden;
        donateQrWrap.hidden = !showQr;
        btnDonateMomo.setAttribute('aria-expanded', showQr ? 'true' : 'false');
        if (donateMomoSub) {
          donateMomoSub.textContent = showQr ? t('popup.donate.momo_hide') : t('popup.donate.momo_sub');
        }
      });
    }
  }

  if (versionBadge && updateDrawer) {
    versionBadge.addEventListener('click', () => {
      setUpdateOpen(updateDrawer.hidden);
    });
  }

  if (btnUpdateClose && updateDrawer) {
    btnUpdateClose.addEventListener('click', () => {
      setUpdateOpen(false);
      if (versionBadge) versionBadge.focus();
    });
  }

  if (btnUpdatePrimary) {
    btnUpdatePrimary.addEventListener('click', async () => {
      if (!currentConnected || updateActionBusy) return;
      const st = currentUpdateState || {};
      const phase = String(st.phase || 'idle');
      const hasUpdate = Boolean(st.has_update);
      const canOneClick = Boolean(st.can_one_click);

      updateActionBusy = true;
      renderUpdateDrawerUI();
      try {
        if (phase === 'downloading') {
          const res = await apiRequest('/app_update', 'POST', { action: 'cancel' });
          if (res && res.ok && res.data) currentUpdateState = res.data;
        } else if (phase === 'ready_to_install' && hasUpdate) {
          const res = await apiRequest('/app_update', 'POST', { action: 'install' });
          if (res && res.ok && res.data) {
            currentUpdateState = res.data;
          } else {
            const errMsg = (res && res.data && res.data.error) || (res && res.error) || 'Install failed';
            showFeedback(t('popup.update.status_error', { error: errMsg }), 'error', 4200);
          }
        } else if (phase === 'available' && hasUpdate && canOneClick) {
          const res = await apiRequest('/app_update', 'POST', {
            action: 'download_and_install',
            auto_install: true
          });
          if (res && res.ok && res.data) {
            currentUpdateState = res.data;
          } else {
            const errMsg = (res && res.data && res.data.error) || (res && res.error) || 'Download failed';
            showFeedback(t('popup.update.status_error', { error: errMsg }), 'error', 4200);
          }
        } else {
          currentUpdateState = { ...(currentUpdateState || {}), phase: 'checking', error: null };
          renderUpdateDrawerUI();
          const res = await apiRequest('/app_update', 'POST', { action: 'check' });
          if (res && res.ok && res.data) {
            currentUpdateState = res.data;
          } else {
            const errMsg = (res && res.data && res.data.error) || (res && res.error) || 'Check failed';
            currentUpdateState = { ...(currentUpdateState || {}), phase: 'error', error: errMsg };
          }
        }
      } finally {
        updateActionBusy = false;
        renderUpdateDrawerUI();
        ensureUpdatePollingIfActive();
        applyState({});
      }
    });
  }

  if (btnUpdateGithub) {
    btnUpdateGithub.addEventListener('click', () => {
      const rawUrl = (currentUpdateState && typeof currentUpdateState.release_url === 'string')
        ? currentUpdateState.release_url
        : '';
      const safeUrl = rawUrl.startsWith('https://github.com/')
        ? rawUrl
        : 'https://github.com/tuanminh26596/Genshin-helper/releases';
      try {
        if (typeof chrome !== 'undefined' && chrome.tabs && typeof chrome.tabs.create === 'function') {
          chrome.tabs.create({ url: safeUrl, active: true });
          return;
        }
      } catch (e) {}
      window.open(safeUrl, '_blank', 'noopener,noreferrer');
    });
  }

  if (selectUpdateMode) {
    selectUpdateMode.addEventListener('change', async () => {
      const wantMode = selectUpdateMode.value;
      const res = await apiRequest('/app_update', 'POST', {
        action: 'set_mode',
        mode: wantMode
      });
      if (res && res.ok && res.data) {
        currentUpdateState = res.data;
        renderUpdateDrawerUI();
        ensureUpdatePollingIfActive();
        applyState({});
      } else {
        showFeedback(t('popup.modes.offline_error'), 'error', 2800);
      }
    });
  }

  if (btnSyncMapData) {
    btnSyncMapData.addEventListener('click', async () => {
      if (!currentConnected) return;
      currentUpdateState = {
        ...(currentUpdateState || {}),
        map_data: { ...((currentUpdateState && currentUpdateState.map_data) || {}), phase: 'checking' }
      };
      renderUpdateDrawerUI();
      const res = await apiRequest('/app_update', 'POST', { action: 'sync_map_data' });
      if (res && res.ok && res.data) {
        currentUpdateState = res.data;
      }
      renderUpdateDrawerUI();
      ensureUpdatePollingIfActive();
    });
  }

  if (btnUninstallExtension) {
    btnUninstallExtension.addEventListener('click', () => {
      try {
        if (typeof chrome !== 'undefined' && chrome.management && typeof chrome.management.uninstallSelf === 'function') {
          chrome.management.uninstallSelf({ showConfirmDialog: true }, () => {
            void (chrome.runtime && chrome.runtime.lastError);
          });
        }
      } catch (e) {}
    });
  }

  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || activeRecordTarget || activeExtRecord) return;
    if (updateDrawer && !updateDrawer.hidden) {
      e.preventDefault();
      setUpdateOpen(false);
      if (versionBadge) versionBadge.focus();
      return;
    }
    if (diagDrawer && !diagDrawer.hidden) {
      e.preventDefault();
      setDiagOpen(false);
      if (btnDiagToggle) btnDiagToggle.focus();
      return;
    }
    if (donateDrawer && !donateDrawer.hidden) {
      e.preventDefault();
      setDonateOpen(false);
      if (btnDonateToggle) btnDonateToggle.focus();
    }
  });

  document.addEventListener('click', (e) => {
    const target = e.target;
    if (updateDrawer && !updateDrawer.hidden) {
      if (!updateDrawer.contains(target) && (!versionBadge || !versionBadge.contains(target))) {
        setUpdateOpen(false);
      }
    }
    if (donateDrawer && !donateDrawer.hidden) {
      if (!donateDrawer.contains(target) && (!btnDonateToggle || !btnDonateToggle.contains(target))) {
        setDonateOpen(false);
      }
    }
    if (diagDrawer && !diagDrawer.hidden) {
      const insideDiag = diagDrawer.contains(target)
        || (btnDiagToggle && btnDiagToggle.contains(target));
      if (!insideDiag) {
        setDiagOpen(false);
      }
    }
  });

  initPopup();
  setInterval(() => {
    if (!powerBusy) {
      refreshFromBg();
    }
  }, 2000);
});
