// Genshin Helper — appsample map bridge (isolated world).
//
// Connects this tab to the extension service worker on the same 'gh-map-tab' port the HoYoLAB
// bridge (content_bridge.js) uses, and forwards live player-position events to the page script
// (appsample_main.js) via window.postMessage. It does not touch HoYoLAB code paths.
(function () {
  'use strict';

  if (window.location.hostname !== 'genshin-impact-map.appsample.com') return;

  const currentRuntimeId = (typeof chrome !== 'undefined' && chrome.runtime && chrome.runtime.id)
    ? chrome.runtime.id
    : null;
  if (window.__ghx_bridge_loaded && window.__ghx_bridge_runtime_id === currentRuntimeId && window.__ghx_bridge_alive) {
    return;
  }
  window.__ghx_bridge_loaded = true;
  window.__ghx_bridge_runtime_id = currentRuntimeId;
  window.__ghx_bridge_alive = true;

  const POSITION_TYPES = { MINIMAP_POSITION: true, LIVE_POSITION: true };

  let port = null;
  let keepaliveTimer = null;
  let reconnectTimer = null;
  let reconnectAttempt = 0;

  function runtimeAlive() {
    return typeof chrome !== 'undefined' && chrome.runtime && chrome.runtime.id;
  }

  function clearKeepalive() {
    if (keepaliveTimer) {
      clearInterval(keepaliveTimer);
      keepaliveTimer = null;
    }
  }

  function scheduleReconnect() {
    if (reconnectTimer) return;
    if (!runtimeAlive()) {
      window.__ghx_bridge_alive = false;
      return;
    }
    const delay = Math.min(5000, Math.round(400 * Math.pow(1.6, reconnectAttempt)));
    reconnectAttempt = Math.min(reconnectAttempt + 1, 8);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connectPort();
    }, delay);
  }

  function toPage(msg) {
    window.postMessage({ __gh_ext: true, dir: 'to-page', ...msg }, window.location.origin);
  }

  function connectPort() {
    if (port || !runtimeAlive()) return;
    try {
      const nextPort = chrome.runtime.connect({ name: 'gh-map-tab' });
      port = nextPort;

      nextPort.onMessage.addListener((msg) => {
        if (!msg || typeof msg !== 'object') return;
        reconnectAttempt = 0;
        if (msg.kind === 'CONNECTION_STATUS') {
          toPage(msg);
        } else if (msg.kind === 'SERVER_EVENT' && msg.data && POSITION_TYPES[msg.data.type]) {
          toPage(msg);
        }
      });

      nextPort.onDisconnect.addListener(() => {
        void chrome.runtime.lastError;
        clearKeepalive();
        if (port === nextPort) port = null;
        if (!runtimeAlive()) {
          window.__ghx_bridge_alive = false;
          return;
        }
        scheduleReconnect();
      });

      keepaliveTimer = setInterval(() => {
        if (!port) return;
        try {
          port.postMessage({ type: 'PING' });
        } catch (err) {
          clearKeepalive();
          port = null;
          scheduleReconnect();
        }
      }, 20000);
    } catch (err) {
      clearKeepalive();
      port = null;
      scheduleReconnect();
    }
  }

  connectPort();
})();
