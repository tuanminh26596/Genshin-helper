(function () {
  'use strict';

  function isSupportedMapPage(loc) {
    if (!loc) return false;
    const host = String(loc.hostname || '').toLowerCase();
    const path = String(loc.pathname || '');
    if (host.includes('hoyolab.com')) return path.includes('/interactive-map/');
    if (host === 'v3.yuanshen.site' || host === 'yuanshen.site') return true;
    if (host === 'genshin-impact-map.appsample.com') return true;
    return false;
  }

  if (!isSupportedMapPage(window.location)) {
    return;
  }

  const currentRuntimeId = (typeof chrome !== 'undefined' && chrome.runtime && chrome.runtime.id)
    ? chrome.runtime.id
    : null;
  if (window.__gh_bridge_loaded && window.__gh_bridge_runtime_id === currentRuntimeId && window.__gh_bridge_alive) {
    if (typeof window.__gh_bridge_reconnect_now === 'function') {
      window.__gh_bridge_reconnect_now();
    }
    return;
  }
  window.__gh_bridge_loaded = true;
  window.__gh_bridge_runtime_id = currentRuntimeId;
  window.__gh_bridge_alive = true;

  let port = null;
  let keepaliveTimer = null;
  let reconnectTimer = null;
  let stableTimer = null;
  let reconnectAttempt = 0;

  function clearPortTimers() {
    if (keepaliveTimer) {
      clearInterval(keepaliveTimer);
      keepaliveTimer = null;
    }
    if (stableTimer) {
      clearTimeout(stableTimer);
      stableTimer = null;
    }
  }

  function scheduleReconnect() {
    if (reconnectTimer) return;
    if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) {
      window.__gh_bridge_alive = false;
      return;
    }
    const delay = Math.min(5000, Math.round(400 * Math.pow(1.6, reconnectAttempt)));
    reconnectAttempt = Math.min(reconnectAttempt + 1, 8);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connectPort();
    }, delay);
  }

  function connectPort() {
    if (port) return;
    if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) {
      return;
    }

    try {
      const nextPort = chrome.runtime.connect({ name: 'gh-map-tab' });
      port = nextPort;

      stableTimer = setTimeout(() => {
        reconnectAttempt = 0;
      }, 3000);

      nextPort.onMessage.addListener((msg) => {
        if (!msg || typeof msg !== 'object') return;
        reconnectAttempt = 0;
        if (msg.kind === 'SERVER_EVENT' || msg.kind === 'CONNECTION_STATUS') {
          window.postMessage(
            {
              __gh_ext: true,
              dir: 'to-page',
              ...msg
            },
            window.location.origin
          );
        }
      });

      nextPort.onDisconnect.addListener(() => {
        void chrome.runtime.lastError;
        clearPortTimers();
        if (port === nextPort) {
          port = null;
        }
        if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) {
          window.__gh_bridge_alive = false;
          return;
        }
        scheduleReconnect();
      });

      keepaliveTimer = setInterval(() => {
        if (!port) return;
        try {
          port.postMessage({ type: 'PING' });
        } catch (err) {
          clearPortTimers();
          port = null;
          scheduleReconnect();
        }
      }, 20000);
    } catch (err) {
      clearPortTimers();
      port = null;
      scheduleReconnect();
    }
  }

  function reconnectNow() {
    if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) {
      window.__gh_bridge_alive = false;
      return false;
    }
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    reconnectAttempt = 0;
    if (!port) {
      connectPort();
    }
    return true;
  }
  window.__gh_bridge_reconnect_now = reconnectNow;

  window.addEventListener('message', (e) => {
    if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-bg') {
      return;
    }
    if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) {
      window.__gh_bridge_alive = false;
      return;
    }

    const { reqId, kind, path, method, body } = e.data;
    if (kind !== 'API_REQUEST' && !path) {
      return;
    }

    try {
      chrome.runtime.sendMessage(
        {
          kind: 'API_REQUEST',
          path,
          method: method || 'GET',
          body
        },
        (response) => {
          const runtimeErr = chrome.runtime.lastError;
          const safeResponse = runtimeErr
            ? { ok: false, status: 0, data: null, error: runtimeErr.message }
            : (response || { ok: false, status: 0, data: null, error: 'Empty response' });

          window.postMessage(
            {
              __gh_ext: true,
              dir: 'to-page',
              kind: 'API_RESPONSE',
              reqId,
              response: safeResponse
            },
            window.location.origin
          );
        }
      );
    } catch (err) {
      window.postMessage(
        {
          __gh_ext: true,
          dir: 'to-page',
          kind: 'API_RESPONSE',
          reqId,
          response: {
            ok: false,
            status: 0,
            data: null,
            error: (err && err.message) ? err.message : 'sendMessage failed'
          }
        },
        window.location.origin
      );
    }
  });

  connectPort();

  window.postMessage(
    {
      __gh_ext: true,
      dir: 'to-page',
      kind: 'BRIDGE_READY'
    },
    window.location.origin
  );
})();
