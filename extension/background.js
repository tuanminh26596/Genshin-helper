'use strict';

importScripts('providers_registry.js');

const API_BASE = 'http://127.0.0.1:8765';
const ALARM_NAME = 'gh-keepalive-poll';
const MAP_DEFAULT_URL = GHMapProviders.getProvider('hoyolab').defaultUrl;
const NATIVE_HOST_NAME = 'com.genshinhelper.host';

function getConnectedProvidersSummary() {
  const providerCounts = {};
  let activeProvider = null;
  for (const port of ports) {
    const tab = port && port.sender && port.sender.tab;
    const pid = (tab && tab.url) ? (GHMapProviders.detectProviderFromUrl(tab.url) || 'hoyolab') : 'hoyolab';
    providerCounts[pid] = (providerCounts[pid] || 0) + 1;
    if (tab && typeof tab.id === 'number' && tab.id === primaryMapTabId) {
      activeProvider = pid;
    }
  }
  if (!activeProvider) {
    const keys = Object.keys(providerCounts);
    if (keys.length > 0) activeProvider = keys[0];
  }
  return { activeProvider, providerCounts };
}

const ports = new Set();
let primaryMapTabId = null;
let lastEventSeq = -1;
let connected = false;
let engineActive = true;
let lastStatus = null;
let lastSync = null;
let lastUpdateState = null;
let postUpdateReconnectTimer = null;
let postUpdateReconnectUntil = 0;
let pollInFlight = false;
let pollTimer = null;
let pollAbortController = null;
let pollGeneration = 0;
let cachedI18nBundles = null;

async function loadI18nBundlesInSw() {
  if (
    cachedI18nBundles &&
    (Object.keys(cachedI18nBundles.vi || {}).length > 0 || Object.keys(cachedI18nBundles.en || {}).length > 0)
  ) {
    return cachedI18nBundles;
  }
  const fetchOne = async (rel) => {
    try {
      const res = await fetch(chrome.runtime.getURL(rel));
      if (!res.ok) return {};
      const parsed = await res.json();
      return (parsed && typeof parsed === 'object') ? parsed : {};
    } catch (e) {
      return {};
    }
  };
  const [vi, en] = await Promise.all([fetchOne('locales/vi.json'), fetchOne('locales/en.json')]);
  if (Object.keys(vi).length > 0 || Object.keys(en).length > 0) {
    cachedI18nBundles = { vi, en };
  }
  return { vi, en };
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function updateBadge(isConnected, isEngineActive = engineActive) {
  try {
    let text = 'OFF';
    let color = '#dc2626';
    if (isConnected) {
      if (isEngineActive) {
        text = 'ON';
        color = '#16a34a';
      } else {
        text = 'ZZZ';
        color = '#d97706';
      }
    }
    chrome.action.setBadgeText({ text });
    chrome.action.setBadgeBackgroundColor({ color });
    if (typeof chrome.action.setBadgeTextColor === 'function') {
      chrome.action.setBadgeTextColor({ color: '#ffffff' });
    }
  } catch (e) {}
}

function broadcastToTabs(payload) {
  for (const port of Array.from(ports)) {
    try {
      port.postMessage(payload);
    } catch (e) {
      ports.delete(port);
    }
  }
}

function setConnectionState(nextConnected) {
  const normalized = Boolean(nextConnected);
  const changed = connected !== normalized;
  connected = normalized;
  updateBadge(connected, engineActive);
  if (changed) {
    broadcastToTabs({ kind: 'CONNECTION_STATUS', connected, engineActive });
  }
}

function setEngineActiveState(nextActive) {
  const normalized = Boolean(nextActive);
  const changed = engineActive !== normalized;
  engineActive = normalized;
  if (lastStatus && typeof lastStatus === 'object') {
    lastStatus = { ...lastStatus, engine_active: engineActive };
  }
  updateBadge(connected, engineActive);
  if (changed) {
    broadcastToTabs({ kind: 'ENGINE_STATE', engineActive });
  }
}

function pickNavHudExtras(data) {
  const out = {};
  if (!data || typeof data !== 'object') return out;
  const pos = typeof data.nav_hud_position === 'string' ? data.nav_hud_position
    : (typeof data.position === 'string' ? data.position : null);
  if (pos) out.nav_hud_position = pos;
  if (typeof data.edit_mode === 'boolean') out.nav_hud_edit_mode = data.edit_mode;
  if (Object.prototype.hasOwnProperty.call(data, 'custom_pos')) out.nav_hud_custom_pos = data.custom_pos;
  if (typeof data.capture_visible === 'boolean') {
    out.nav_hud_capture_visible = data.capture_visible;
  } else if (typeof data.nav_hud_capture_visible === 'boolean') {
    out.nav_hud_capture_visible = data.nav_hud_capture_visible;
  }
  const zoomMode = typeof data.nav_hud_guide_zoom_mode === 'string' ? data.nav_hud_guide_zoom_mode
    : (typeof data.guide_zoom_mode === 'string' ? data.guide_zoom_mode : null);
  if (zoomMode) out.nav_hud_guide_zoom_mode = zoomMode;
  if (typeof data.mmap_overlay_mode === 'string' && data.mmap_overlay_mode) {
    out.mmap_overlay_mode = data.mmap_overlay_mode;
  }
  return out;
}

function syncStatusCacheFromEvent(data) {
  if (!data || !data.type) return;
  if (!lastStatus) lastStatus = {};

  if (typeof data.uipi_mismatch === 'boolean') {
    lastStatus = { ...lastStatus, uipi_mismatch: data.uipi_mismatch };
  } else if (typeof data.uipi_blocked === 'boolean') {
    lastStatus = { ...lastStatus, uipi_mismatch: data.uipi_blocked };
  }

  if (data.type === 'AUTO_SYNC_UPDATED' && typeof data.auto_sync_m === 'boolean') {
    lastStatus = { ...lastStatus, auto_sync_m: data.auto_sync_m };
  } else if (data.type === 'SINGLE_MONITOR_UPDATED' && typeof data.single_monitor_mode === 'boolean') {
    lastStatus = { ...lastStatus, single_monitor_mode: data.single_monitor_mode };
  } else if (data.type === 'ENGINE_STATE_UPDATED' && typeof data.engine_active === 'boolean') {
    setEngineActiveState(data.engine_active);
  } else if (data.type === 'LANGUAGE_UPDATED' && typeof data.language === 'string' && data.language) {
    lastStatus = { ...lastStatus, language: data.language };
  } else if (data.type === 'MINIMAP_TRACKING_UPDATED') {
    const val = typeof data.minimap_tracking === 'boolean'
      ? data.minimap_tracking
      : (typeof data.enabled === 'boolean' ? data.enabled : null);
    if (val !== null) {
      lastStatus = { ...lastStatus, minimap_tracking: val };
    }
  } else if (data.type === 'NAV_HUD_UPDATED') {
    const val = typeof data.nav_hud_enabled === 'boolean'
      ? data.nav_hud_enabled
      : (typeof data.enabled === 'boolean' ? data.enabled : null);
    if (val !== null) {
      lastStatus = { ...lastStatus, nav_hud_enabled: val };
    }
    lastStatus = { ...lastStatus, ...pickNavHudExtras(data) };
  } else if (data.type === 'RADAR_UPDATED') {
    const next = { ...lastStatus };
    const enabled = typeof data.radar_enabled === 'boolean'
      ? data.radar_enabled
      : (typeof data.enabled === 'boolean' ? data.enabled : null);
    if (enabled !== null) next.radar_enabled = enabled;
    if (typeof data.radar_zoom === 'number') next.radar_zoom = data.radar_zoom;
    if (typeof data.radar_opacity === 'number') next.radar_opacity = data.radar_opacity;
    if (Array.isArray(data.radar_rect)) next.radar_rect = data.radar_rect;
    lastStatus = next;
  } else if (data.type === 'LAN_COMPANION_UPDATED') {
    const running = typeof data.lan_companion_running === 'boolean'
      ? data.lan_companion_running
      : (typeof data.is_running === 'boolean'
          ? data.is_running
          : (typeof data.running === 'boolean' ? data.running : null));
    const next = { ...lastStatus };
    if (running !== null) next.lan_companion_running = running;
    const info = (data.info && typeof data.info === 'object') ? data.info : (data.url || data.pin || data.qr_svg ? data : null);
    if (info) {
      next.lan_companion = info;
    } else if (running === false) {
      next.lan_companion = { is_running: false };
    }
    lastStatus = next;
  } else if (data.type === 'AUDIO_UPDATED') {
    const next = { ...lastStatus };
    if (typeof data.audio_enabled === 'boolean') next.audio_enabled = data.audio_enabled;
    else if (typeof data.enabled === 'boolean') next.audio_enabled = data.enabled;
    if (typeof data.audio_volume === 'number') next.audio_volume = data.audio_volume;
    else if (typeof data.volume === 'number') next.audio_volume = data.volume;
    lastStatus = next;
  } else if (data.type === 'UIPI_STATUS_UPDATED') {
    const mismatch = typeof data.uipi_mismatch === 'boolean'
      ? data.uipi_mismatch
      : Boolean(data.uipi_blocked || data.needs_elevation);
    lastStatus = { ...lastStatus, uipi_mismatch: mismatch };
  } else if (data.type === 'APP_UPDATE_UPDATED') {
    applyAppUpdateSnapshot(data);
  } else if (data.type === 'HOTKEY_UPDATED') {
    const act = data.action || 'sync';
    const next = { ...lastStatus };
    if (data.hotkey) next[`hotkey_${act}`] = data.hotkey;
    if (data.display_name) next[`display_name_${act}`] = data.display_name;
    lastStatus = next;
  } else if ((data.type === 'SYNC_POSITION_ONLY' || data.type === 'SYNC_AND_MARK') && data.has_world_coords === true) {
    lastSync = {
      map_id: data.map_id ?? 2,
      map_name: data.map_name || '',
      area_name: data.area_name || '',
      lat: data.lat,
      lng: data.lng,
      is_underground: Boolean(data.is_underground),
      floor: data.floor_short || data.floor_name || 'G',
      ts: Date.now() / 1000
    };
  } else if ((data.type === 'MINIMAP_POSITION' || data.type === 'LIVE_POSITION') && typeof data.lat === 'number' && typeof data.lng === 'number') {
    const prev = lastSync || {};
    const nextMapId = data.map_id ?? prev.map_id ?? 2;
    const sameMap = prev.map_id === nextMapId;
    lastSync = {
      map_id: nextMapId,
      map_name: data.map_name || (sameMap ? prev.map_name : '') || '',
      area_name: data.area_name || (sameMap ? prev.area_name : '') || '',
      lat: data.lat,
      lng: data.lng,
      is_underground: typeof data.is_underground === 'boolean' ? data.is_underground : Boolean(sameMap && prev.is_underground),
      floor: data.floor_short || data.floor_name || (sameMap ? prev.floor : 'G') || 'G',
      ts: Date.now() / 1000
    };
  }
}

let autoReloadScheduledForVersion = '';

function stopPostUpdateReconnectWatch() {
  if (postUpdateReconnectTimer) {
    clearInterval(postUpdateReconnectTimer);
    postUpdateReconnectTimer = null;
  }
  postUpdateReconnectUntil = 0;
}

function startPostUpdateReconnectWatch() {
  postUpdateReconnectUntil = Date.now() + 65000;
  if (postUpdateReconnectTimer) return;
  postUpdateReconnectTimer = setInterval(() => {
    if (Date.now() > postUpdateReconnectUntil) {
      stopPostUpdateReconnectWatch();
      return;
    }
    refreshStatusQuietly().then((res) => {
      if (res && res.ok && res.data && typeof res.data.version === 'string') {
        maybeAutoReloadForAppUpgrade(res.data.version);
      }
    });
  }, 1500);
}

function applyAppUpdateSnapshot(data) {
  if (!data || typeof data !== 'object') return;
  lastUpdateState = { ...data };
  const mapData = (data.map_data && typeof data.map_data === 'object') ? data.map_data : {};
  const summary = {
    phase: typeof data.phase === 'string' ? data.phase : 'idle',
    has_update: Boolean(data.has_update),
    latest_version: typeof data.latest_version === 'string' && data.latest_version ? data.latest_version : null,
    can_one_click: Boolean(data.can_one_click),
    verified_ready: Boolean(data.verified_ready),
    progress_pct: typeof data.progress_pct === 'number' ? data.progress_pct : 0,
    app_update_mode: typeof data.app_update_mode === 'string'
      ? data.app_update_mode
      : (lastStatus && typeof lastStatus.app_update_mode === 'string' ? lastStatus.app_update_mode : 'auto'),
    auto_check_app_updates: typeof data.auto_check_app_updates === 'boolean'
      ? data.auto_check_app_updates
      : (lastStatus && typeof lastStatus.auto_check_app_updates === 'boolean' ? lastStatus.auto_check_app_updates : true),
    will_install_on_exit: Boolean(data.will_install_on_exit),
    auto_install_blocked: Boolean(data.auto_install_blocked),
    just_updated_from: typeof data.just_updated_from === 'string' && data.just_updated_from ? data.just_updated_from : null,
    map_data_phase: typeof mapData.phase === 'string' ? mapData.phase : 'idle',
    map_restart_recommended: Boolean(mapData.restart_recommended)
  };
  lastStatus = {
    ...(lastStatus || {}),
    app_update_mode: summary.app_update_mode,
    auto_check_app_updates: summary.auto_check_app_updates,
    app_update: summary
  };
  if (data.phase === 'installing') {
    startPostUpdateReconnectWatch();
  }
}

function parseSemverTuple(ver) {
  if (typeof ver !== 'string') return null;
  const clean = ver.trim().replace(/^v/i, '').split('-')[0];
  const parts = clean.split('.').map((p) => Number.parseInt(p, 10));
  if (parts.length < 2 || parts.some((n) => !Number.isFinite(n) || n < 0)) return null;
  return [parts[0] || 0, parts[1] || 0, parts[2] || 0];
}

function compareSemver(a, b) {
  const ta = parseSemverTuple(a);
  const tb = parseSemverTuple(b);
  if (!ta || !tb) return 0;
  for (let i = 0; i < 3; i += 1) {
    if (ta[i] !== tb[i]) return ta[i] - tb[i];
  }
  return 0;
}

function maybeAutoReloadForAppUpgrade(serverVersion) {
  if (typeof serverVersion !== 'string' || !serverVersion.trim()) return;
  let extVer = '';
  try {
    const manifest = chrome.runtime && typeof chrome.runtime.getManifest === 'function'
      ? chrome.runtime.getManifest()
      : null;
    extVer = manifest && manifest.version ? String(manifest.version) : '';
  } catch (e) {
    return;
  }
  if (!extVer || compareSemver(serverVersion, extVer) <= 0) return;

  const target = serverVersion.trim().replace(/^v/i, '');
  if (autoReloadScheduledForVersion === target) return;
  autoReloadScheduledForVersion = target;

  const triggerOnce = () => {
    setTimeout(() => {
      try {
        if (chrome.runtime && typeof chrome.runtime.reload === 'function') {
          chrome.runtime.reload();
        }
      } catch (e) {}
    }, 1200);
  };

  try {
    if (chrome.storage && chrome.storage.session && typeof chrome.storage.session.get === 'function') {
      chrome.storage.session.get({ gh_auto_reloaded_ver: '' }, (stored) => {
        void (chrome.runtime && chrome.runtime.lastError);
        if (stored && stored.gh_auto_reloaded_ver === target) return;
        chrome.storage.session.set({ gh_auto_reloaded_ver: target }, () => {
          void (chrome.runtime && chrome.runtime.lastError);
          triggerOnce();
        });
      });
      return;
    }
  } catch (e) {}

  triggerOnce();
}

function syncStatusCacheFromApi(path, ok, data) {
  if (!ok || !data || typeof data !== 'object') return;
  if (path.startsWith('/status')) {
    const normalized = { ...data };
    if (typeof normalized.uipi_mismatch !== 'boolean' && typeof normalized.uipi_blocked === 'boolean') {
      normalized.uipi_mismatch = normalized.uipi_blocked;
    }
    if (typeof normalized.lan_companion_running !== 'boolean') {
      if (normalized.lan_companion && typeof normalized.lan_companion.is_running === 'boolean') {
        normalized.lan_companion_running = normalized.lan_companion.is_running;
      } else if (typeof normalized.lan_companion_enabled === 'boolean') {
        normalized.lan_companion_running = normalized.lan_companion_enabled;
      }
    }
    lastStatus = normalized;
    if (typeof data.version === 'string' && data.version) {
      maybeAutoReloadForAppUpgrade(data.version);
    }
    if (typeof data.engine_active === 'boolean') {
      engineActive = data.engine_active;
    }
    if (Object.prototype.hasOwnProperty.call(data, 'last_sync')) {
      lastSync = data.last_sync || null;
    }
    if (typeof data.latest_seq === 'number') {
      if (lastEventSeq < 0 || data.latest_seq < lastEventSeq) {
        lastEventSeq = data.latest_seq;
      }
    }
  } else if (path.startsWith('/set_engine_state') && typeof data.engine_active === 'boolean') {
    engineActive = data.engine_active;
    lastStatus = { ...(lastStatus || {}), engine_active: engineActive };
  } else if (path.startsWith('/set_auto_sync') && typeof data.auto_sync_m === 'boolean') {
    lastStatus = { ...(lastStatus || {}), auto_sync_m: data.auto_sync_m };
  } else if (path.startsWith('/set_single_monitor') && typeof data.single_monitor_mode === 'boolean') {
    lastStatus = { ...(lastStatus || {}), single_monitor_mode: data.single_monitor_mode };
  } else if (path.startsWith('/set_language') && typeof data.language === 'string' && data.language) {
    lastStatus = { ...(lastStatus || {}), language: data.language };
  } else if (path.startsWith('/set_minimap_tracking')) {
    const val = typeof data.minimap_tracking === 'boolean'
      ? data.minimap_tracking
      : (typeof data.enabled === 'boolean' ? data.enabled : null);
    if (val !== null) {
      lastStatus = { ...(lastStatus || {}), minimap_tracking: val };
    }
  } else if (path.startsWith('/set_nav_hud')) {
    const val = typeof data.nav_hud_enabled === 'boolean'
      ? data.nav_hud_enabled
      : (typeof data.enabled === 'boolean' ? data.enabled : null);
    if (val !== null) {
      lastStatus = { ...(lastStatus || {}), nav_hud_enabled: val };
    }
    lastStatus = { ...(lastStatus || {}), ...pickNavHudExtras(data) };
  } else if (path.startsWith('/set_radar')) {
    const next = { ...(lastStatus || {}) };
    const enabled = typeof data.radar_enabled === 'boolean'
      ? data.radar_enabled
      : (typeof data.enabled === 'boolean' ? data.enabled : null);
    if (enabled !== null) next.radar_enabled = enabled;
    if (typeof data.radar_zoom === 'number') next.radar_zoom = data.radar_zoom;
    if (typeof data.radar_opacity === 'number') next.radar_opacity = data.radar_opacity;
    if (Array.isArray(data.radar_rect)) next.radar_rect = data.radar_rect;
    lastStatus = next;
  } else if (path.startsWith('/set_audio')) {
    const next = { ...(lastStatus || {}) };
    if (typeof data.audio_enabled === 'boolean') next.audio_enabled = data.audio_enabled;
    if (typeof data.audio_volume === 'number') next.audio_volume = data.audio_volume;
    lastStatus = next;
  } else if (path.startsWith('/lan_companion')) {
    const info = (data.info && typeof data.info === 'object') ? data.info : data;
    const running = typeof data.lan_companion_running === 'boolean'
      ? data.lan_companion_running
      : (typeof info.is_running === 'boolean'
          ? info.is_running
          : (typeof data.is_running === 'boolean'
              ? data.is_running
              : (typeof data.running === 'boolean' ? data.running : null)));
    const next = { ...(lastStatus || {}) };
    if (running !== null) next.lan_companion_running = running;
    next.lan_companion = running === false ? { is_running: false } : info;
    lastStatus = next;
  } else if (path.startsWith('/set_hotkey') && data.results && typeof data.results === 'object') {
    const next = { ...(lastStatus || {}) };
    for (const act of ['sync', 'debug', 'switch', 'nav_mark', 'nav_undo', 'nav_guide', 'nav_guide_next', 'nav_next', 'nav_layout', 'nav_toggle', 'radar_toggle']) {
      const r = data.results[act];
      if (r && r.ok) {
        if (r.hotkey) next[`hotkey_${act}`] = r.hotkey;
        if (r.display_name) next[`display_name_${act}`] = r.display_name;
      }
    }
    lastStatus = next;
  } else if (path.startsWith('/remap_external_hotkey') && data.ok && data.claim_updated && typeof data.claim_updated === 'object') {
    const cu = data.claim_updated;
    if (cu.action) {
      const next = { ...(lastStatus || {}) };
      if (cu.hotkey) next[`hotkey_${cu.action}`] = cu.hotkey;
      if (cu.display_name) next[`display_name_${cu.action}`] = cu.display_name;
      lastStatus = next;
    }
  } else if (path.startsWith('/app_update')) {
    applyAppUpdateSnapshot(data);
  }
}

async function fetchApi(path, method = 'GET', body = undefined, timeoutMs = 12000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const fetchOpts = {
      method: method || 'GET',
      cache: 'no-store',
      signal: controller.signal
    };
    if (body !== undefined && fetchOpts.method !== 'GET' && fetchOpts.method !== 'HEAD') {
      fetchOpts.headers = { 'Content-Type': 'application/json' };
      fetchOpts.body = typeof body === 'string' ? body : JSON.stringify(body);
    }

    const res = await fetch(`${API_BASE}${path}`, fetchOpts);
    const text = await res.text();
    let parsed = null;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch (e) {
        parsed = { raw: text };
      }
    }

    syncStatusCacheFromApi(path, res.ok, parsed);
    setConnectionState(true);
    return {
      ok: res.ok,
      status: res.status,
      data: parsed,
      error: res.ok ? null : (parsed && parsed.error) || `HTTP ${res.status}`
    };
  } catch (err) {
    setConnectionState(false);
    return {
      ok: false,
      status: 0,
      data: null,
      error: (err && err.message) ? err.message : 'Network error'
    };
  } finally {
    clearTimeout(timer);
  }
}

function sendNativeHostMessage(payload) {
  return new Promise((resolve) => {
    try {
      if (typeof chrome === 'undefined' || !chrome.runtime || typeof chrome.runtime.sendNativeMessage !== 'function') {
        resolve({ ok: false, error: 'Native messaging API unavailable', lastError: true });
        return;
      }
      chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, payload, (response) => {
        const err = chrome.runtime.lastError;
        if (err) {
          resolve({ ok: false, error: err.message || 'Native host error', lastError: true });
          return;
        }
        if (!response || typeof response !== 'object') {
          resolve({ ok: false, error: 'Empty native host response' });
          return;
        }
        resolve(response);
      });
    } catch (e) {
      resolve({ ok: false, error: (e && e.message) ? e.message : 'sendNativeMessage failed', lastError: true });
    }
  });
}

async function refreshStatusQuietly() {
  return fetchApi('/status', 'GET', undefined, 2500);
}

async function handleSetEnginePower(active) {
  const wantActive = Boolean(active);

  if (!wantActive) {
    const res = await fetchApi('/set_engine_state', 'POST', { active: false }, 2500);
    if (res && res.ok) {
      setEngineActiveState(Boolean(res.data && typeof res.data.engine_active === 'boolean' ? res.data.engine_active : false));
      return { ok: true, engine_active: engineActive, via: 'http' };
    }
    engineActive = false;
    updateBadge(connected, false);
    return { ok: true, engine_active: false, alreadyOffline: true };
  }

  // active === true: first try HTTP wake from Standby
  const httpRes = await fetchApi('/set_engine_state', 'POST', { active: true }, 2500);
  if (httpRes && httpRes.ok) {
    setEngineActiveState(Boolean(httpRes.data && typeof httpRes.data.engine_active === 'boolean' ? httpRes.data.engine_active : true));
    await refreshStatusQuietly();
    ensurePolling();
    return { ok: true, engine_active: engineActive, via: 'http' };
  }

  // HTTP failed (Desktop App completely closed) -> launch via Native Messaging Host
  const nativeRes = await sendNativeHostMessage({ cmd: 'start_server' });
  if (!nativeRes || !nativeRes.ok) {
    return {
      ok: false,
      needNativeHost: true,
      error: (nativeRes && nativeRes.error) ? nativeRes.error : 'Native host unavailable'
    };
  }

  // Wait briefly for HTTP server on 127.0.0.1:8765 to accept connections
  let statusRes = null;
  for (let attempt = 0; attempt < 6; attempt++) {
    await sleep(attempt === 0 ? 300 : 450);
    statusRes = await refreshStatusQuietly();
    if (statusRes && statusRes.ok) {
      break;
    }
  }

  if (statusRes && statusRes.ok) {
    if (statusRes.data && statusRes.data.engine_active === false) {
      await fetchApi('/set_engine_state', 'POST', { active: true }, 2500);
    } else {
      setEngineActiveState(true);
    }
    ensurePolling();
    return { ok: true, engine_active: true, via: 'native_host' };
  }

  return {
    ok: false,
    error: 'Đã khởi động app nhưng server HTTP chưa kịp phản hồi, hãy thử lại sau vài giây.'
  };
}

async function handleHardShutdown(preferNative = false) {
  stopPolling();
  let via = null;
  let httpRes = null;
  let nativeRes = null;

  if (!preferNative) {
    httpRes = await fetchApi('/shutdown', 'POST', {}, 3000);
    if (httpRes && httpRes.ok) {
      via = 'http';
    }
  }

  if (!via) {
    nativeRes = await sendNativeHostMessage({ cmd: 'stop_server', mode: 'shutdown' });
    if (nativeRes && nativeRes.ok) {
      via = 'native_host';
    }
  }

  connected = false;
  engineActive = false;
  lastEventSeq = -1;
  if (lastStatus && typeof lastStatus === 'object') {
    lastStatus = { ...lastStatus, engine_active: false };
  }
  updateBadge(false, false);
  broadcastToTabs({ kind: 'CONNECTION_STATUS', connected: false, engineActive: false });

  if (ports.size > 0) {
    setTimeout(() => {
      if (ports.size > 0 && !connected) {
        ensurePolling();
      }
    }, 2000);
  }

  if (via) {
    return { ok: true, via };
  }
  return {
    ok: false,
    alreadyOffline: true,
    error: (nativeRes && nativeRes.error) || (httpRes && httpRes.error) || 'Desktop app is already offline'
  };
}

function stopPolling() {
  pollGeneration += 1;
  if (pollTimer) {
    clearTimeout(pollTimer);
    pollTimer = null;
  }
  if (pollAbortController) {
    try {
      pollAbortController.abort();
    } catch (e) {}
    pollAbortController = null;
  }
  pollInFlight = false;
}

function scheduleNextPoll(gen, delayMs) {
  if (gen !== pollGeneration || ports.size === 0) return;
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = setTimeout(() => {
    pollTimer = null;
    pollLoop(gen);
  }, delayMs);
}

async function focusBestMapTab(targetProvider = null) {
  try {
    const tabs = await chrome.tabs.query({ url: GHMapProviders.ALL_TAB_QUERY_URLS });
    if (!Array.isArray(tabs) || tabs.length === 0) return;

    const wantedProvider = (typeof targetProvider === 'string' && targetProvider.trim())
      ? targetProvider.trim().toLowerCase()
      : ((lastStatus && typeof lastStatus.active_map_provider === 'string' && lastStatus.active_map_provider.trim())
          ? lastStatus.active_map_provider.trim().toLowerCase()
          : null);

    // Multi-browser guard: when the desktop app explicitly requests a specific provider
    // (e.g. Edge has AppSample while Chrome has HoYoLAB), a browser that does NOT host any
    // tab for `wantedProvider` MUST NOT call windows.update(focused:true) and steal focus.
    if (wantedProvider) {
      const hasMatchingProvider = tabs.some(
        (t) => t && typeof t.url === 'string' && GHMapProviders.detectProviderFromUrl(t.url) === wantedProvider
      );
      if (!hasMatchingProvider) return;
    }

    const connectedTabIds = new Set();
    for (const port of ports) {
      const tid = port && port.sender && port.sender.tab && port.sender.tab.id;
      if (typeof tid === 'number') connectedTabIds.add(tid);
    }

    const scoreTab = (t) => {
      const url = String((t && t.url) || '');
      const prov = GHMapProviders.detectProviderFromUrl(url);
      let score = 0;
      if (prov) score += 100;
      if (wantedProvider && prov === wantedProvider) score += 80;
      if (typeof primaryMapTabId === 'number' && t && t.id === primaryMapTabId) score += 40;
      if (connectedTabIds.has(t.id)) score += 20;
      if (t.active) score += 5;
      return score;
    };

    let best = tabs[0];
    let bestScore = scoreTab(best);
    for (let i = 1; i < tabs.length; i++) {
      const s = scoreTab(tabs[i]);
      if (s > bestScore) {
        best = tabs[i];
        bestScore = s;
      }
    }
    if (!best || typeof best.id !== 'number') return;

    if (!best.active) {
      await chrome.tabs.update(best.id, { active: true });
    }
    if (typeof best.windowId === 'number') {
      await chrome.windows.update(best.windowId, { focused: true }).catch(() => {});
    }
  } catch (e) {
    // Best-effort tab focus
  }
}

async function pollLoop(gen) {
  if (gen !== pollGeneration || ports.size === 0 || pollInFlight) return;
  pollInFlight = true;

  const controller = new AbortController();
  pollAbortController = controller;
  const timeoutId = setTimeout(() => controller.abort(), 20000);

  let nextDelay = 20;
  try {
    const res = await fetch(`${API_BASE}/poll?since=${lastEventSeq}`, {
      cache: 'no-store',
      signal: controller.signal
    });
    if (gen !== pollGeneration) return;
    if (!res.ok) throw new Error(`HTTP ${res.status}`);

    const data = await res.json();
    if (gen !== pollGeneration) return;

    const wasConnected = connected;

    if (data && typeof data._seq === 'number') {
      if (data.type === 'HEARTBEAT') {
        if (lastEventSeq < 0 || data._seq < lastEventSeq) {
          lastEventSeq = data._seq;
        }
      } else {
        lastEventSeq = data._seq;
      }
    }

    if (data && data.type === 'SHUTDOWN') {
      engineActive = false;
      setConnectionState(false);
      lastEventSeq = -1;
      nextDelay = 1000;
    } else {
      if (data && typeof data.engine_active === 'boolean') {
        engineActive = data.engine_active;
      }
      if (data && data.type && data.type !== 'HEARTBEAT') {
        syncStatusCacheFromEvent(data);
        setConnectionState(true);
        if (data.type === 'FOCUS_MAP_TAB') {
          focusBestMapTab(data.target_provider || null);
        }
        if (data.type === 'DIAGNOSTIC_CAPTURED' && data.folder_name) {
          attachHoYoLABViewportToBundle(data.folder_name);
        }
        if (data.type === 'UNINSTALL_EXTENSION') {
          try {
            if (typeof chrome !== 'undefined' && chrome.management && typeof chrome.management.uninstallSelf === 'function') {
              chrome.management.uninstallSelf({ showConfirmDialog: true }, () => {
                void (chrome.runtime && chrome.runtime.lastError);
              });
            }
          } catch (e) {}
        }
        broadcastToTabs({ kind: 'SERVER_EVENT', data });
      } else {
        setConnectionState(true);
      }
      if (!wasConnected) {
        refreshStatusQuietly();
      }
    }
  } catch (err) {
    if (gen !== pollGeneration) return;
    setConnectionState(false);
    nextDelay = 1000;
  } finally {
    clearTimeout(timeoutId);
    if (pollAbortController === controller) {
      pollAbortController = null;
    }
    pollInFlight = false;
  }

  if (gen === pollGeneration && ports.size > 0) {
    scheduleNextPoll(gen, nextDelay);
  }
}

function ensurePolling() {
  if (ports.size === 0) return;
  if (pollInFlight || pollTimer) return;
  const gen = ++pollGeneration;
  pollLoop(gen);
}

function ensureAlarm() {
  try {
    chrome.alarms.create(ALARM_NAME, { periodInMinutes: 1 });
  } catch (e) {}
}

function getConnectedPortForTab(tabId) {
  if (typeof tabId !== 'number') return null;
  for (const p of ports) {
    if (p && p.sender && p.sender.tab && p.sender.tab.id === tabId) return p;
  }
  return null;
}

function setPrimaryMapTab(tabId, requestRefresh = false) {
  if (typeof tabId !== 'number') return;
  const changed = primaryMapTabId !== tabId;
  primaryMapTabId = tabId;
  if (changed && requestRefresh) {
    const port = getConnectedPortForTab(tabId);
    if (port) {
      try {
        port.postMessage({
          kind: 'SERVER_EVENT',
          data: { type: 'GH_CMD_REFRESH_PINS', _local_cmd: true }
        });
      } catch (e) {}
    }
  }
}

function resolvePrimaryMapTabId() {
  if (typeof primaryMapTabId === 'number' && getConnectedPortForTab(primaryMapTabId)) {
    return primaryMapTabId;
  }
  const activePorts = Array.from(ports);
  const chosen = activePorts.find((p) => p && p.sender && p.sender.tab && p.sender.tab.active)
    || activePorts[activePorts.length - 1]
    || null;
  primaryMapTabId = (chosen && chosen.sender && chosen.sender.tab && typeof chosen.sender.tab.id === 'number')
    ? chosen.sender.tab.id
    : null;
  return primaryMapTabId;
}

try {
  if (typeof chrome !== 'undefined' && chrome.tabs && chrome.tabs.onActivated) {
    chrome.tabs.onActivated.addListener((info) => {
      if (info && typeof info.tabId === 'number' && getConnectedPortForTab(info.tabId)) {
        setPrimaryMapTab(info.tabId, true);
      }
    });
  }
  if (typeof chrome !== 'undefined' && chrome.windows && chrome.windows.onFocusChanged) {
    chrome.windows.onFocusChanged.addListener((winId) => {
      if (typeof winId !== 'number' || winId < 0) return;
      try {
        chrome.tabs.query({ active: true, windowId: winId }, (tabs) => {
          void (chrome.runtime && chrome.runtime.lastError);
          const t = Array.isArray(tabs) ? tabs[0] : null;
          if (t && typeof t.id === 'number' && getConnectedPortForTab(t.id)) {
            setPrimaryMapTab(t.id, true);
          }
        });
      } catch (e) {}
    });
  }
} catch (e) {}

chrome.runtime.onConnect.addListener((port) => {
  if (!port || port.name !== 'gh-map-tab') return;
  const isFirstTab = ports.size === 0;
  ports.add(port);
  const tabId = (port.sender && port.sender.tab && typeof port.sender.tab.id === 'number')
    ? port.sender.tab.id
    : null;
  if (tabId !== null && (primaryMapTabId === null || (port.sender.tab && port.sender.tab.active))) {
    primaryMapTabId = tabId;
  }

  try {
    port.postMessage({ kind: 'CONNECTION_STATUS', connected, engineActive });
  } catch (e) {}

  port.onMessage.addListener((msg) => {
    if (msg && msg.type === 'PING') {
      ensurePolling();
    }
  });

  port.onDisconnect.addListener(() => {
    ports.delete(port);
    if (tabId !== null && primaryMapTabId === tabId) {
      primaryMapTabId = null;
      const nextPrimary = resolvePrimaryMapTabId();
      if (nextPrimary !== null) {
        const nextPort = getConnectedPortForTab(nextPrimary);
        if (nextPort) {
          try {
            nextPort.postMessage({
              kind: 'SERVER_EVENT',
              data: { type: 'GH_CMD_REFRESH_PINS', _local_cmd: true }
            });
          } catch (e) {}
        }
      }
    }
    if (ports.size === 0) {
      stopPolling();
    }
  });

  if (isFirstTab && !lastStatus) {
    refreshStatusQuietly().finally(() => ensurePolling());
  } else {
    ensurePolling();
  }
});

let nonForceReinjectPromise = null;

async function tabHasLiveBridge(tabId) {
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId },
      func: () => {
        const alive = Boolean(
          window.__gh_bridge_alive &&
          typeof chrome !== 'undefined' &&
          chrome.runtime &&
          chrome.runtime.id &&
          window.__gh_bridge_runtime_id === chrome.runtime.id
        );
        if (alive && typeof window.__gh_bridge_reconnect_now === 'function') {
          try {
            window.__gh_bridge_reconnect_now();
          } catch (e) {}
        }
        return alive;
      }
    });
    return Boolean(results && results[0] && results[0].result === true);
  } catch (e) {
    return false;
  }
}

function reinjectOpenMapTabs(opts = {}) {
  const forceAll = Boolean(opts && opts.forceAll);
  if (!forceAll && nonForceReinjectPromise) {
    return nonForceReinjectPromise;
  }
  const run = new Promise((resolve) => {
    try {
      if (
        typeof chrome === 'undefined' ||
        !chrome.tabs ||
        typeof chrome.tabs.query !== 'function' ||
        !chrome.scripting ||
        typeof chrome.scripting.executeScript !== 'function'
      ) {
        resolve({ ok: false, injected: 0, reconnected: 0 });
        return;
      }
      const connectedTabIds = new Set();
      for (const p of ports) {
        if (p && p.sender && p.sender.tab && typeof p.sender.tab.id === 'number') {
          connectedTabIds.add(p.sender.tab.id);
        }
      }
      chrome.tabs.query({ url: GHMapProviders.ALL_TAB_QUERY_URLS }, (tabs) => {
        void chrome.runtime.lastError;
        const targets = Array.isArray(tabs)
          ? tabs.filter(
              (t) =>
                t &&
                typeof t.id === 'number' &&
                typeof t.url === 'string' &&
                GHMapProviders.isSupportedMapUrl(t.url) &&
                (forceAll || !connectedTabIds.has(t.id))
            )
          : [];
        if (targets.length === 0) {
          resolve({ ok: true, injected: 0, reconnected: 0 });
          return;
        }
        let injected = 0;
        let reconnected = 0;
        Promise.all(
          targets.map(async (tab) => {
            try {
              if (!forceAll && (await tabHasLiveBridge(tab.id))) {
                reconnected += 1;
                return;
              }
              await chrome.scripting.executeScript({
                target: { tabId: tab.id },
                files: ['content_bridge.js', 'i18n_bridge.js']
              });
              await chrome.scripting.executeScript({
                target: { tabId: tab.id },
                world: 'MAIN',
                files: ['i18n.js', 'coord_transform.js', 'provider_runtime.js', 'page_main.js']
              });
              injected += 1;
            } catch (e) {}
          })
        ).then(() => resolve({ ok: true, injected, reconnected }));
      });
    } catch (e) {
      resolve({ ok: false, injected: 0, reconnected: 0 });
    }
  });
  if (!forceAll) {
    nonForceReinjectPromise = run;
    run.finally(() => {
      if (nonForceReinjectPromise === run) {
        nonForceReinjectPromise = null;
      }
    });
  }
  return run;
}

function forwardCommandToMapTab(cmdType, payload = {}, broadcastAll = false, opts = {}) {
  return new Promise(async (resolve) => {
    if (ports.size === 0) {
      await reinjectOpenMapTabs();
      if (ports.size === 0) {
        await sleep(220);
      }
    }
    const requiredProvider = (opts && typeof opts.requiredProvider === 'string' && opts.requiredProvider.trim())
      ? opts.requiredProvider.trim().toLowerCase()
      : null;
    const preferredProvider = requiredProvider
      || (opts && typeof opts.preferredProvider === 'string' && opts.preferredProvider.trim()
          ? opts.preferredProvider.trim().toLowerCase()
          : ((lastStatus && typeof lastStatus.active_map_provider === 'string' && lastStatus.active_map_provider.trim())
              ? lastStatus.active_map_provider.trim().toLowerCase()
              : null));

    const allPorts = Array.from(ports);
    const activePorts = requiredProvider
      ? allPorts.filter((p) => {
          const url = p && p.sender && p.sender.tab && p.sender.tab.url;
          return GHMapProviders.detectProviderFromUrl(url) === requiredProvider;
        })
      : allPorts;
    if (activePorts.length === 0) {
      resolve({
        ok: false,
        noTab: true,
        error: requiredProvider === 'hoyolab'
          ? 'No HoYoLAB Map tab connected'
          : 'No supported Map tab connected'
      });
      return;
    }

    const envelope = {
      kind: 'SERVER_EVENT',
      data: {
        ...payload,
        type: cmdType,
        _local_cmd: true
      }
    };

    if (broadcastAll) {
      let sent = 0;
      for (const port of activePorts) {
        try {
          port.postMessage(envelope);
          sent += 1;
        } catch (e) {
          ports.delete(port);
        }
      }
      resolve(sent > 0 ? { ok: true, count: sent } : { ok: false, noTab: true, error: 'No HoYoLAB Map tab connected' });
      return;
    }

    const pickAndSend = (preferredTabId) => {
      let chosen = null;
      if (typeof preferredTabId === 'number') {
        chosen = activePorts.find((p) => p && p.sender && p.sender.tab && p.sender.tab.id === preferredTabId) || null;
      }
      if (!chosen && preferredProvider) {
        chosen = activePorts.find((p) => {
          const url = p && p.sender && p.sender.tab && p.sender.tab.url;
          return GHMapProviders.detectProviderFromUrl(url) === preferredProvider;
        }) || null;
      }
      if (!chosen) {
        chosen = activePorts.find((p) => p && p.sender && p.sender.tab && p.sender.tab.active) || activePorts[activePorts.length - 1];
      }
      if (!chosen) {
        resolve({ ok: false, noTab: true, error: 'No HoYoLAB Map tab connected' });
        return;
      }
      try {
        chosen.postMessage(envelope);
        const tabId = (chosen.sender && chosen.sender.tab && typeof chosen.sender.tab.id === 'number')
          ? chosen.sender.tab.id
          : null;
        resolve({ ok: true, tabId });
      } catch (e) {
        ports.delete(chosen);
        resolve({ ok: false, noTab: true, error: (e && e.message) ? e.message : 'Failed to post to map tab' });
      }
    };

    try {
      chrome.tabs.query({ url: GHMapProviders.ALL_TAB_QUERY_URLS }, (tabs) => {
        void chrome.runtime.lastError;
        const filtered = Array.isArray(tabs)
          ? tabs.filter((t) => {
              if (!t || typeof t.url !== 'string') return false;
              const p = GHMapProviders.detectProviderFromUrl(t.url);
              if (!p) return false;
              return !requiredProvider || p === requiredProvider;
            })
          : [];
        const activeMapTab =
          (preferredProvider && filtered.find((t) => t.active && GHMapProviders.detectProviderFromUrl(t.url) === preferredProvider)) ||
          (preferredProvider && filtered.find((t) => t.id === primaryMapTabId && GHMapProviders.detectProviderFromUrl(t.url) === preferredProvider)) ||
          (preferredProvider && filtered.find((t) => GHMapProviders.detectProviderFromUrl(t.url) === preferredProvider)) ||
          filtered.find((t) => t.active) ||
          filtered.find((t) => typeof t.id === 'number' && t.id === primaryMapTabId) ||
          filtered[0] ||
          null;
        pickAndSend(activeMapTab && typeof activeMapTab.id === 'number' ? activeMapTab.id : null);
      });
    } catch (e) {
      pickAndSend(null);
    }
  });
}

function getSanitizedExtensionInfo(extra = {}) {
  let extVersion = '5.8.0';
  try {
    const manifest = chrome.runtime && typeof chrome.runtime.getManifest === 'function'
      ? chrome.runtime.getManifest()
      : null;
    if (manifest && manifest.version) {
      extVersion = String(manifest.version);
    }
  } catch (e) {}

  let browserFamily = 'Chromium';
  try {
    const ua = (typeof navigator !== 'undefined' && navigator.userAgent) ? String(navigator.userAgent) : '';
    const edg = ua.match(/Edg\/(\d+)/);
    const opr = ua.match(/OPR\/(\d+)/);
    const chr = ua.match(/Chrome\/(\d+)/);
    if (edg) browserFamily = `Edge ${edg[1]}`;
    else if (opr) browserFamily = `Opera ${opr[1]}`;
    else if (chr) browserFamily = `Chrome/Chromium ${chr[1]}`;
  } catch (e) {}

  return {
    extension_version: extVersion,
    browser_family: browserFamily,
    connected_map_tabs: ports.size,
    compact_ui_enabled: Boolean(extra.compact_ui_enabled),
    lang: typeof extra.lang === 'string' && extra.lang ? extra.lang : 'vi'
  };
}

function captureHoYoLABMapViewport() {
  return new Promise((resolve) => {
    try {
      if (
        typeof chrome === 'undefined' ||
        !chrome.tabs ||
        typeof chrome.tabs.query !== 'function' ||
        typeof chrome.tabs.captureVisibleTab !== 'function'
      ) {
        resolve({ ok: false, reason: 'capture_api_unavailable' });
        return;
      }

      chrome.tabs.query({ active: true, url: GHMapProviders.ALL_TAB_QUERY_URLS }, (tabs) => {
        const qErr = chrome.runtime && chrome.runtime.lastError;
        if (qErr) {
          resolve({ ok: false, reason: 'tab_query_error' });
          return;
        }
        const candidates = Array.isArray(tabs)
          ? tabs.filter(
              (t) =>
                t &&
                typeof t.id === 'number' &&
                typeof t.windowId === 'number' &&
                typeof t.url === 'string' &&
                GHMapProviders.isSupportedMapUrl(t.url)
            )
          : [];
        if (candidates.length === 0) {
          resolve({ ok: false, reason: 'no_active_map_tab' });
          return;
        }

        const pickTab = async () => {
          const activeProv = (lastStatus && typeof lastStatus.active_map_provider === 'string' && lastStatus.active_map_provider.trim())
            ? lastStatus.active_map_provider.trim().toLowerCase()
            : null;
          const orderedCandidates = activeProv
            ? [
                ...candidates.filter((t) => GHMapProviders.detectProviderFromUrl(t.url) === activeProv),
                ...candidates.filter((t) => GHMapProviders.detectProviderFromUrl(t.url) !== activeProv)
              ]
            : candidates;
          let chosen = orderedCandidates[0];
          for (const t of orderedCandidates) {
            try {
              const win = await new Promise((resWin) => {
                chrome.windows.get(t.windowId, (w) => {
                  void (chrome.runtime && chrome.runtime.lastError);
                  resWin(w || null);
                });
              });
              if (win && win.focused && win.state !== 'minimized') {
                chosen = t;
                break;
              }
            } catch (e) {}
          }

          const targetWin = await new Promise((resWin) => {
            chrome.windows.get(chosen.windowId, (w) => {
              void (chrome.runtime && chrome.runtime.lastError);
              resWin(w || null);
            });
          });
          if (!targetWin || targetWin.state === 'minimized') {
            resolve({ ok: false, reason: 'window_minimized' });
            return;
          }

          const chosenProvider = GHMapProviders.detectProviderFromUrl(chosen.url) || 'hoyolab';
          const privacyCss = GHMapProviders.getPrivacyMaskCss(chosenProvider);

          const applyPrivacyMask = async () => {
            if (!chrome.scripting || typeof chrome.scripting.executeScript !== 'function') return false;
            try {
              await chrome.scripting.executeScript({
                target: { tabId: chosen.id },
                args: [privacyCss],
                func: (cssText) => {
                  const old = document.getElementById('gh-diag-privacy-mask');
                  if (old) old.remove();
                  const st = document.createElement('style');
                  st.id = 'gh-diag-privacy-mask';
                  st.textContent = cssText;
                  (document.head || document.documentElement).appendChild(st);
                  return new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
                }
              });
              return true;
            } catch (e) {
              return false;
            }
          };

          const removePrivacyMask = async () => {
            if (!chrome.scripting || typeof chrome.scripting.executeScript !== 'function') return;
            try {
              await chrome.scripting.executeScript({
                target: { tabId: chosen.id },
                func: () => {
                  const el = document.getElementById('gh-diag-privacy-mask');
                  if (el) el.remove();
                }
              });
            } catch (e) {}
          };

          await applyPrivacyMask();
          try {
            const dataUrl = await new Promise((resCap) => {
              chrome.tabs.captureVisibleTab(chosen.windowId, { format: 'png' }, (url) => {
                const capErr = chrome.runtime && chrome.runtime.lastError;
                if (capErr || !url || typeof url !== 'string' || !url.startsWith('data:image/png;base64,')) {
                  resCap(null);
                  return;
                }
                resCap(url);
              });
            });
            if (dataUrl) {
              resolve({ ok: true, dataUrl });
            } else {
              resolve({ ok: false, reason: 'capture_failed' });
            }
          } finally {
            await removePrivacyMask();
          }
        };

        pickTab().catch(() => resolve({ ok: false, reason: 'capture_exception' }));
      });
    } catch (e) {
      resolve({ ok: false, reason: 'capture_exception' });
    }
  });
}

async function attachHoYoLABViewportToBundle(folderName) {
  if (!folderName || typeof folderName !== 'string' || !folderName.startsWith('diag_')) {
    return { ok: false, skipped: true };
  }
  const cap = await captureHoYoLABMapViewport();
  if (!cap || !cap.ok || !cap.dataUrl) {
    return { ok: false, reason: (cap && cap.reason) || 'capture_skipped' };
  }
  return fetchApi(
    '/attach_diagnostic_browser',
    'POST',
    { folder_name: folderName, png_base64: cap.dataUrl },
    25000
  );
}

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (!msg || typeof msg !== 'object') return false;

  if (msg.kind === 'API_REQUEST') {
    const path = typeof msg.path === 'string' ? msg.path : '';
    if (!path.startsWith('/')) {
      sendResponse({ ok: false, status: 0, data: null, error: 'Invalid API path' });
      return false;
    }
    const senderTabId = (sender && sender.tab && typeof sender.tab.id === 'number') ? sender.tab.id : null;
    if (senderTabId !== null) {
      if (path === '/nav_command') {
        setPrimaryMapTab(senderTabId, false);
      } else if (
        ports.size > 1 &&
        (path === '/nearby_pins' || path === '/lan_pins' || path === '/report_state')
      ) {
        const primaryId = resolvePrimaryMapTabId();
        if (primaryId !== null && senderTabId !== primaryId) {
          sendResponse({ ok: true, status: 200, data: { ok: true, ignored_secondary_tab: true } });
          return false;
        }
      }
    }
    const method = (msg.method || 'GET').toUpperCase();
    const timeoutMs = path.startsWith('/cross_map_import_preview')
      ? 90000
      : (path.startsWith('/cross_map_export')
          ? 30000
          : ((path.startsWith('/trigger_diagnostic') || path.startsWith('/attach_diagnostic_browser'))
              ? 25000
              : (path.startsWith('/app_update') || path.startsWith('/cross_map_import_history') ? 20000 : 12000)));
    fetchApi(path, method, msg.body, timeoutMs).then(sendResponse);
    return true;
  }

  if (msg.kind === 'TRIGGER_DIAGNOSTIC_FLOW') {
    (async () => {
      const includeBrowser = msg.includeBrowserCapture !== false;
      const cap = includeBrowser
        ? await captureHoYoLABMapViewport()
        : { ok: false, reason: 'disabled_by_user' };
      const extInfo = getSanitizedExtensionInfo({
        compact_ui_enabled: Boolean(msg.compactUiEnabled),
        lang: msg.lang
      });
      const payload = {
        source: 'extension_popup',
        note: typeof msg.note === 'string' ? msg.note : '',
        category: typeof msg.category === 'string' ? msg.category : '',
        extension_info: extInfo
      };
      if (cap && cap.ok && cap.dataUrl) {
        payload.browser_png_base64 = cap.dataUrl;
      }
      const res = await fetchApi('/trigger_diagnostic', 'POST', payload, 25000);
      return {
        ...res,
        browserCaptured: Boolean(cap && cap.ok),
        browserCaptureSkippedReason: (cap && !cap.ok) ? (cap.reason || null) : null
      };
    })().then(sendResponse);
    return true;
  }

  if (msg.kind === 'GH_CMD_PLAN_TSP') {
    forwardCommandToMapTab('GH_CMD_PLAN_TSP', msg.payload || {}, false).then(sendResponse);
    return true;
  }

  if (msg.kind === 'GH_CMD_CLEAR_TSP') {
    forwardCommandToMapTab('GH_CMD_CLEAR_TSP', msg.payload || {}, true).then(sendResponse);
    return true;
  }

  if (msg.kind === 'GH_CMD_EXPORT_PINS') {
    const targetFormat = typeof msg.targetFormat === 'string' ? msg.targetFormat : 'canonical';
    forwardCommandToMapTab('GH_CMD_EXPORT_PINS', { targetFormat }, false, { requiredProvider: 'hoyolab' }).then(sendResponse);
    return true;
  }

  if (msg.kind === 'OPEN_IMPORTER_TAB') {
    const targetUrl = chrome.runtime.getURL('importer.html');
    chrome.tabs.query({ url: targetUrl }, (tabs) => {
      void chrome.runtime.lastError;
      const existing = Array.isArray(tabs) && tabs.length > 0 ? tabs[0] : null;
      if (existing && typeof existing.id === 'number') {
        chrome.windows.update(existing.windowId, { focused: true }, () => {
          void chrome.runtime.lastError;
          chrome.tabs.update(existing.id, { active: true }, () => {
            const err = chrome.runtime.lastError;
            sendResponse(err ? { ok: false, error: err.message } : { ok: true, tabId: existing.id, created: false });
          });
        });
      } else {
        chrome.tabs.create({ url: targetUrl, active: true }, (created) => {
          const err = chrome.runtime.lastError;
          sendResponse(err ? { ok: false, error: err.message } : { ok: true, tabId: created ? created.id : null, created: true });
        });
      }
    });
    return true;
  }

  if (msg.kind === 'READ_APPSAMPLE_TAB') {
    (async () => {
      const tabs = await new Promise((res) => {
        chrome.tabs.query({ url: 'https://genshin-impact-map.appsample.com/*' }, (list) => {
          void chrome.runtime.lastError;
          res(Array.isArray(list) ? list : []);
        });
      });
      const tab = tabs.find((t) => t && t.active) || tabs[0];
      if (!tab || typeof tab.id !== 'number') {
        return { ok: false, noTab: true, error: 'No open Appsample tab found' };
      }
      const results = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        world: 'MAIN',
        func: () => {
          const toIntList = (src) => {
            const out = new Set();
            if (!src) return [];
            if (src instanceof Set) {
              for (const v of src) {
                const n = Number(v);
                if (Number.isInteger(n)) out.add(n);
              }
            } else if (Array.isArray(src)) {
              for (const v of src) {
                const n = Number(v);
                if (Number.isInteger(n)) out.add(n);
              }
            } else if (typeof src === 'object') {
              for (const [k, v] of Object.entries(src)) {
                if (v) {
                  const n = Number(k);
                  if (Number.isInteger(n)) out.add(n);
                }
              }
            }
            return Array.from(out);
          };

          const findFiberRoots = () => {
            const candidates = [
              document,
              document.documentElement,
              document.getElementById('__next'),
              document.getElementById('root'),
              document.body,
              ...(document.body ? Array.from(document.body.children).slice(0, 8) : [])
            ].filter(Boolean);
            const roots = [];
            for (const el of candidates) {
              for (const key of Object.keys(el)) {
                if (key.startsWith('__reactContainer$') || key.startsWith('__reactFiber$')) {
                  roots.push(el[key]);
                }
              }
            }
            return roots;
          };

          let foundIds = [];
          let catalogAll = null;
          let sourceMode = 'none';

          const roots = findFiberRoots();
          if (roots.length > 0) {
            const queue = [...roots];
            const visited = new Set();
            let steps = 0;
            while (queue.length > 0 && steps < 8000) {
              const node = queue.shift();
              if (!node || typeof node !== 'object' || visited.has(node)) continue;
              visited.add(node);
              steps += 1;

              const val = node.memoizedProps && node.memoizedProps.value;
              if (val && typeof val === 'object') {
                if (val.store && typeof val.store.getState === 'function') {
                  try {
                    const st = val.store.getState();
                    if (st && st.found) {
                      const ids = toIntList(st.found);
                      if (ids.length >= foundIds.length) {
                        foundIds = ids;
                        sourceMode = 'react_fiber';
                      }
                    }
                  } catch (e) {}
                }
                if (val.catalog && Array.isArray(val.catalog.all) && val.catalog.all.length > 0) {
                  catalogAll = val.catalog.all;
                }
              }
              if (foundIds.length > 0 && catalogAll) break;
              if (node.child) queue.push(node.child);
              if (node.sibling) queue.push(node.sibling);
              if (node.return) queue.push(node.return);
            }
          }

          // Fallback for guest / localStorage progress
          if (foundIds.length === 0) {
            try {
              const rawLocal = localStorage.getItem('gim-local-markers');
              if (rawLocal) {
                const parsed = JSON.parse(rawLocal);
                const ids = toIntList(parsed && (parsed.found || parsed.ids || parsed));
                if (ids.length > 0) {
                  foundIds = ids;
                  sourceMode = 'local_storage';
                }
              }
            } catch (e) {}
          }

          // Keep only user's marked markers + o3 Teleports (for live Multi-Affine refinement)
          // so IPC + localhost JSON stays compact (~200 KB instead of 12 MB).
          const foundSet = new Set(foundIds);
          const markers = [];
          if (Array.isArray(catalogAll)) {
            for (const m of catalogAll) {
              if (!m || typeof m !== 'object') continue;
              const id = Number(m.id);
              if (!Number.isInteger(id)) continue;
              const typeStr = String(m.type || '');
              if (!foundSet.has(id) && typeStr !== 'o3') continue;
              const lng = Number(
                m.lng !== undefined ? m.lng : (m.position ? m.position.x : NaN)
              );
              const lat = Number(
                m.lat !== undefined ? m.lat : (m.position ? m.position.y : NaN)
              );
              if (!Number.isFinite(lng) || !Number.isFinite(lat)) continue;
              const mid = Number(m.mapId ?? m.mid ?? m.map_id ?? m.map ?? 2);
              let meta = String(m.meta || '');
              if (!meta && m.layer && typeof m.layer === 'object') {
                const gid = Number(m.layer.groupId || 0);
                const fid = Number(m.layer.floorId || 0);
                if (gid > 0 || fid > 0) {
                  meta = `l=0|${gid}|${fid}`;
                }
              }
              markers.push({
                id,
                type: typeStr,
                mid: Number.isInteger(mid) && mid > 0 ? mid : 2,
                level: Number(m.level ?? 0) || 0,
                lng,
                lat,
                meta
              });
            }
          }

          return {
            ok: true,
            sourceMode,
            foundCount: foundIds.length,
            catalogCount: markers.length,
            foundIds,
            markers
          };
        }
      });
      const payload = results && results[0] && results[0].result;
      if (!payload || !payload.ok) {
        return { ok: false, error: 'Failed to inspect Appsample tab state' };
      }
      return payload;
    })().then(sendResponse).catch((err) => {
      sendResponse({ ok: false, error: (err && err.message) ? err.message : 'Appsample tab read failed' });
    });
    return true;
  }

  if (msg.kind === 'READ_KONGYING_TAB') {
    (async () => {
      const tabs = await new Promise((res) => {
        chrome.tabs.query({ url: ['https://v3.yuanshen.site/*', 'https://yuanshen.site/*'] }, (list) => {
          void chrome.runtime.lastError;
          res(Array.isArray(list) ? list : []);
        });
      });
      const tab = tabs.find((t) => t && t.active) || tabs[0];
      if (!tab || typeof tab.id !== 'number') {
        return { ok: false, noTab: true, error: 'No open Kongying Tavern tab found' };
      }
      const results = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        world: 'MAIN',
        func: () => {
          const out = new Set();
          const ingest = (raw) => {
            if (!raw) return;
            try {
              const parsed = typeof raw === 'string' ? JSON.parse(raw) : raw;
              const arr = Array.isArray(parsed)
                ? parsed
                : (parsed && Array.isArray(parsed.Data_KYJG) ? parsed.Data_KYJG : Object.keys(parsed || {}));
              for (const item of arr) {
                const n = Number(item);
                if (Number.isInteger(n) && n > 0) out.add(n);
              }
            } catch (e) {}
          };
          try {
            ingest(localStorage.getItem('marked_layers'));
            if (out.size === 0) ingest(localStorage.getItem('Data_KYJG'));
          } catch (e) {}
          const ids = Array.from(out).sort((a, b) => a - b);
          return { ok: true, count: ids.length, ids };
        }
      });
      const payload = results && results[0] && results[0].result;
      if (!payload || !payload.ok) {
        return { ok: false, error: 'Failed to read Kongying Tavern tab' };
      }
      return payload;
    })().then(sendResponse).catch((err) => {
      sendResponse({ ok: false, error: (err && err.message) ? err.message : 'Kongying tab read failed' });
    });
    return true;
  }

  if (msg.kind === 'HOYOLAB_GET_MARKED') {
    (async () => {
      const tabs = await new Promise((res) => {
        chrome.tabs.query({ url: 'https://act.hoyolab.com/*' }, (list) => {
          void chrome.runtime.lastError;
          res(Array.isArray(list) ? list : []);
        });
      });
      const mapTab =
        tabs.find((t) => t && t.active && typeof t.url === 'string' && t.url.includes('/interactive-map/')) ||
        tabs.find((t) => t && typeof t.url === 'string' && t.url.includes('/interactive-map/'));
      if (!mapTab || typeof mapTab.id !== 'number') {
        return { ok: false, noTab: true, error: 'No open HoYoLAB Interactive Map tab found' };
      }
      const mapIds = Array.isArray(msg.mapIds) && msg.mapIds.length > 0
        ? msg.mapIds.map((x) => Number(x)).filter((x) => Number.isInteger(x) && x > 0)
        : [2, 7, 9, 34, 36, 37, 40];
      const results = await chrome.scripting.executeScript({
        target: { tabId: mapTab.id },
        world: 'MAIN',
        args: [mapIds],
        func: async (ids) => {
          const base = 'https://sg-public-api.hoyolab.com/common/map_user/ys_obc';
          const markedByMap = {};
          for (const mid of ids) {
            const url = `${base}/v1/map/point/mark_map_point_list?map_id=${encodeURIComponent(mid)}&app_sn=ys_obc&lang=en-us`;
            const resp = await fetch(url, {
              method: 'GET',
              credentials: 'include',
              headers: { 'x-rpc-map_version': '4.5' }
            });
            const json = await resp.json().catch(() => null);
            if (!json || json.retcode !== 0) {
              return {
                ok: false,
                loggedIn: false,
                retcode: json ? json.retcode : resp.status,
                message: (json && json.message) || `HTTP ${resp.status}`
              };
            }
            const rawList = (json.data && (json.data.list || json.data.point_list)) || [];
            const clean = [];
            for (const item of rawList) {
              const pid = Number(typeof item === 'object' && item !== null ? (item.point_id ?? item.id) : item);
              if (Number.isInteger(pid) && pid > 0) clean.push(pid);
            }
            markedByMap[String(mid)] = clean;
          }
          return { ok: true, loggedIn: true, markedByMap };
        }
      });
      const payload = results && results[0] && results[0].result;
      if (!payload) return { ok: false, error: 'HoYoLAB marked list query failed' };
      return { ...payload, tabId: mapTab.id };
    })().then(sendResponse).catch((err) => {
      sendResponse({ ok: false, error: (err && err.message) ? err.message : 'HoYoLAB query failed' });
    });
    return true;
  }

  if (msg.kind === 'HOYOLAB_BATCH_MARK') {
    (async () => {
      const tabs = await new Promise((res) => {
        chrome.tabs.query({ url: 'https://act.hoyolab.com/*' }, (list) => {
          void chrome.runtime.lastError;
          res(Array.isArray(list) ? list : []);
        });
      });
      const mapTab =
        (typeof msg.tabId === 'number' && tabs.find((t) => t && t.id === msg.tabId)) ||
        tabs.find((t) => t && t.active && typeof t.url === 'string' && t.url.includes('/interactive-map/')) ||
        tabs.find((t) => t && typeof t.url === 'string' && t.url.includes('/interactive-map/'));
      if (!mapTab || typeof mapTab.id !== 'number') {
        return { ok: false, noTab: true, error: 'HoYoLAB Interactive Map tab was closed' };
      }
      const mapId = Number(msg.mapId) || 2;
      const pointIds = Array.isArray(msg.pointIds)
        ? msg.pointIds.map((x) => Number(x)).filter((x) => Number.isInteger(x) && x > 0)
        : [];
      const isDelete = Boolean(msg.isDelete);
      if (pointIds.length === 0) {
        return { ok: true, status: 200, retcode: 0, message: 'OK', ms: 0 };
      }
      const results = await chrome.scripting.executeScript({
        target: { tabId: mapTab.id },
        world: 'MAIN',
        args: [mapId, pointIds, isDelete],
        func: async (mid, ids, del) => {
          const t0 = performance.now();
          const resp = await fetch(
            'https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/batch_mark_map_point?app_sn=ys_obc&lang=en-us',
            {
              method: 'POST',
              credentials: 'include',
              headers: {
                'Content-Type': 'application/json',
                'x-rpc-map_version': '4.5'
              },
              body: JSON.stringify({
                map_id: mid,
                app_sn: 'ys_obc',
                lang: 'en-us',
                list: ids.map((id) => ({ point_id: id, is_delete: del }))
              })
            }
          );
          const json = await resp.json().catch(() => null);
          const ms = Math.round(performance.now() - t0);
          const ok = resp.ok && json && json.retcode === 0;
          return {
            ok,
            status: resp.status,
            retcode: json ? json.retcode : null,
            message: (json && json.message) || `HTTP ${resp.status}`,
            ms
          };
        }
      });
      const payload = results && results[0] && results[0].result;
      return payload || { ok: false, error: 'HoYoLAB batch_mark call failed' };
    })().then(sendResponse).catch((err) => {
      sendResponse({ ok: false, error: (err && err.message) ? err.message : 'HoYoLAB batch_mark failed' });
    });
    return true;
  }

  if (msg.kind === 'HOYOLAB_REFRESH_AFTER_IMPORT') {
    (async () => {
      const tabs = await new Promise((res) => {
        chrome.tabs.query({ url: 'https://act.hoyolab.com/*' }, (list) => {
          void chrome.runtime.lastError;
          res(Array.isArray(list) ? list : []);
        });
      });
      const mapTab =
        tabs.find((t) => t && t.active && typeof t.url === 'string' && t.url.includes('/interactive-map/')) ||
        tabs.find((t) => t && typeof t.url === 'string' && t.url.includes('/interactive-map/'));
      if (!mapTab || typeof mapTab.id !== 'number') {
        return { ok: false, noTab: true };
      }
      if (msg.reload) {
        await new Promise((res) => {
          chrome.tabs.reload(mapTab.id, {}, () => {
            void chrome.runtime.lastError;
            res();
          });
        });
        return { ok: true, reloaded: true };
      }
      await chrome.scripting.executeScript({
        target: { tabId: mapTab.id },
        world: 'MAIN',
        func: () => {
          if (window.__GH_BRIDGE__ && typeof window.__GH_BRIDGE__.refreshMarkedPins === 'function') {
            window.__GH_BRIDGE__.refreshMarkedPins();
          }
        }
      });
      return { ok: true, reloaded: false };
    })().then(sendResponse).catch(() => sendResponse({ ok: false }));
    return true;
  }

  if (msg.kind === 'GH_CMD_ROUTE_EXPORT' || msg.kind === 'GH_CMD_ROUTE_IMPORT' || msg.kind === 'GH_CMD_ROUTE_CLEAR') {
    forwardCommandToMapTab(msg.kind, msg.payload || {}, msg.kind === 'GH_CMD_ROUTE_CLEAR').then(sendResponse);
    return true;
  }

  if (msg.kind === 'GH_CMD_OPEN_ROUTE_URL') {
    const rawUrl = typeof msg.url === 'string' ? msg.url.trim() : '';
    if (!rawUrl.startsWith('https://act.hoyolab.com/ys/app/interactive-map/')) {
      sendResponse({ ok: false, error: 'Invalid HoYoLAB route share URL' });
      return false;
    }
    try {
      chrome.tabs.query({ url: 'https://act.hoyolab.com/*' }, (tabs) => {
        void chrome.runtime.lastError;
        const mapTab = Array.isArray(tabs)
          ? (
              tabs.find((t) => t && t.active && typeof t.url === 'string' && t.url.includes('/interactive-map/')) ||
              tabs.find((t) => t && typeof t.url === 'string' && t.url.includes('/interactive-map/'))
            )
          : null;
        if (mapTab && typeof mapTab.id === 'number') {
          chrome.tabs.update(mapTab.id, { url: rawUrl, active: true }, () => {
            void chrome.runtime.lastError;
            if (typeof mapTab.windowId === 'number') {
              chrome.windows.update(mapTab.windowId, { focused: true }, () => { void chrome.runtime.lastError; });
            }
            sendResponse({ ok: true, tabId: mapTab.id, updated: true });
          });
        } else {
          chrome.tabs.create({ url: rawUrl, active: true }, (tab) => {
            void chrome.runtime.lastError;
            sendResponse({ ok: true, tabId: tab && tab.id ? tab.id : null, created: true });
          });
        }
      });
    } catch (err) {
      sendResponse({ ok: false, error: (err && err.message) ? err.message : 'Failed to open route URL' });
    }
    return true;
  }

  if (msg.kind === 'GET_I18N_BUNDLES') {
    loadI18nBundlesInSw().then((bundles) => {
      sendResponse({ ok: true, bundles });
    });
    return true;
  }

  if (msg.kind === 'RECONNECT_MAP_TABS') {
    reinjectOpenMapTabs({ forceAll: true }).then(async (res) => {
      if (res.injected > 0 && ports.size === 0) {
        await sleep(220);
      }
      sendResponse({ ...res, tabCount: ports.size });
    });
    return true;
  }

  if (msg.kind === 'GET_STATE') {
    const buildStatePayload = () => {
      const prov = getConnectedProvidersSummary();
      return {
        connected,
        engineActive,
        lastStatus,
        lastSync,
        lastUpdateState,
        tabCount: ports.size,
        activeProvider: prov.activeProvider,
        providerCounts: prov.providerCounts
      };
    };
    if (ports.size === 0) {
      reinjectOpenMapTabs().then(async (res) => {
        if ((res.injected > 0 || res.reconnected > 0) && ports.size === 0) {
          await sleep(180);
        }
        sendResponse(buildStatePayload());
      });
      return true;
    }
    sendResponse(buildStatePayload());
    return false;
  }

  if (msg.kind === 'SET_ENGINE_POWER') {
    handleSetEnginePower(Boolean(msg.active)).then(sendResponse);
    return true;
  }

  if (msg.kind === 'HARD_SHUTDOWN') {
    handleHardShutdown(Boolean(msg.preferNative)).then(sendResponse);
    return true;
  }

  if (msg.kind === 'OPEN_MAP_TAB') {
    const reqProvider = typeof msg.providerId === 'string' && msg.providerId.trim()
      ? GHMapProviders.getProvider(msg.providerId)
      : null;
    const queryUrls = reqProvider ? [reqProvider.tabQueryUrl] : GHMapProviders.ALL_TAB_QUERY_URLS;
    const defaultUrl = reqProvider ? reqProvider.defaultUrl : MAP_DEFAULT_URL;
    chrome.tabs.query({ url: queryUrls }, (tabs) => {
      if (chrome.runtime.lastError) {
        sendResponse({ ok: false, error: chrome.runtime.lastError.message });
        return;
      }
      const mapTab = Array.isArray(tabs)
        ? tabs.find((t) =>
            t && typeof t.url === 'string' &&
            (reqProvider ? reqProvider.matchesUrl(t.url) : GHMapProviders.isSupportedMapUrl(t.url))
          )
        : null;
      if (mapTab && typeof mapTab.id === 'number') {
        chrome.windows.update(mapTab.windowId, { focused: true }, () => {
          void chrome.runtime.lastError;
          chrome.tabs.update(mapTab.id, { active: true }, () => {
            const err = chrome.runtime.lastError;
            sendResponse(err ? { ok: false, error: err.message } : { ok: true, created: false, tabId: mapTab.id });
          });
        });
      } else {
        chrome.tabs.create({ url: defaultUrl, active: true }, (created) => {
          const err = chrome.runtime.lastError;
          sendResponse(err ? { ok: false, error: err.message } : { ok: true, created: true, tabId: created ? created.id : null });
        });
      }
    });
    return true;
  }

  return false;
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (!alarm || alarm.name !== ALARM_NAME) return;
  if (ports.size > 0) {
    ensurePolling();
  } else {
    refreshStatusQuietly();
  }
});

chrome.runtime.onInstalled.addListener(() => {
  updateBadge(false, false);
  ensureAlarm();
  refreshStatusQuietly();
  reinjectOpenMapTabs();
});

chrome.runtime.onStartup.addListener(() => {
  updateBadge(false, false);
  ensureAlarm();
  refreshStatusQuietly();
  reinjectOpenMapTabs();
});

updateBadge(false, false);
ensureAlarm();
refreshStatusQuietly();
reinjectOpenMapTabs();
