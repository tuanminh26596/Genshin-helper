// Isolated-world i18n bridge on the HoYoLAB Interactive Map.
//
// Relays locale bundles (from the service worker, immune to Brave Shields / page CSP), the
// popup's gh_lang and gh_compact_ui between chrome.storage and the MAIN-world i18n.js /
// page_main.js via window.postMessage.
//
// Kept in its own file on purpose: Chromium injects each manifest content-script FILE at most
// once per frame, across all content_scripts entries and worlds. When i18n.js was listed in both
// the isolated entry and the MAIN entry, only the isolated copy ran, so window.GHI18n never
// existed in the page and every page.* label showed its raw key.
(function () {
  'use strict';

  if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) return;
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
  // Allow clean re-injection after an extension reload when the prior isolated context was invalidated
  if (window.__gh_i18n_isolated_check && window.__gh_i18n_isolated_check()) {
    return;
  }
  window.__gh_i18n_isolated_loaded = true;
  window.__gh_i18n_isolated_check = () => Boolean(typeof chrome !== 'undefined' && chrome.runtime && chrome.runtime.id);

  const STORAGE_KEY_LANG = 'gh_lang';
  const STORAGE_KEY_COMPACT = 'gh_compact_ui';

  // Stored languages are written already normalised ('vi' / 'en'); anything else is "no
  // preference" and is left to the MAIN-world i18n.js (browser language / desktop app).
  function storedLang(raw) {
    return raw === 'vi' || raw === 'en' ? raw : null;
  }

  function storageGet(keys) {
    return new Promise((resolve) => {
      try {
        if (chrome.storage && chrome.storage.local) {
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
        if (chrome.storage && chrome.storage.local) {
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

  let cachedBundles = null;
  let cachedLang = null;

  function requestBundlesFromBg() {
    return new Promise((resolve) => {
      try {
        chrome.runtime.sendMessage({ kind: 'GET_I18N_BUNDLES' }, (resp) => {
          void (chrome.runtime && chrome.runtime.lastError);
          if (resp && resp.ok && resp.bundles && typeof resp.bundles === 'object') {
            resolve(resp.bundles);
            return;
          }
          resolve(null);
        });
      } catch (e) {
        resolve(null);
      }
    });
  }

  async function ensureBundlesLoaded() {
    if (cachedBundles && (Object.keys(cachedBundles.vi).length > 0 || Object.keys(cachedBundles.en).length > 0)) {
      return cachedBundles;
    }
    // Prefer Service Worker delivery first (100% immune to Brave Shields & cross-origin page CSP)
    const bgBundles = await requestBundlesFromBg();
    if (bgBundles && (Object.keys(bgBundles.vi || {}).length > 0 || Object.keys(bgBundles.en || {}).length > 0)) {
      cachedBundles = bgBundles;
      return cachedBundles;
    }
    const [vi, en] = await Promise.all([
      fetchJsonSafe(chrome.runtime.getURL('locales/vi.json')),
      fetchJsonSafe(chrome.runtime.getURL('locales/en.json'))
    ]);
    const fetched = { vi, en };
    if (Object.keys(vi).length > 0 || Object.keys(en).length > 0) {
      cachedBundles = fetched;
    }
    return cachedBundles || fetched;
  }

  async function pushStateToPage(explicitLang) {
    const [bundles, stored] = await Promise.all([
      ensureBundlesLoaded(),
      storageGet([STORAGE_KEY_LANG, STORAGE_KEY_COMPACT])
    ]);
    const lang = storedLang(explicitLang) || storedLang(stored[STORAGE_KEY_LANG]);
    cachedLang = lang;

    window.postMessage({
      __gh_ext: true,
      dir: 'to-page',
      kind: 'I18N_SYNC',
      lang,
      bundles
    }, window.location.origin);

    if (typeof stored[STORAGE_KEY_COMPACT] === 'boolean') {
      window.postMessage({
        __gh_ext: true,
        dir: 'to-page',
        kind: 'COMPACT_UI_SYNC',
        enabled: stored[STORAGE_KEY_COMPACT]
      }, window.location.origin);
    }
  }

  if (chrome.storage && chrome.storage.onChanged) {
    chrome.storage.onChanged.addListener((changes, area) => {
      if (area !== 'local' || !changes) return;
      if (changes[STORAGE_KEY_LANG]) {
        pushStateToPage(changes[STORAGE_KEY_LANG].newValue);
      }
      if (changes[STORAGE_KEY_COMPACT] && typeof changes[STORAGE_KEY_COMPACT].newValue === 'boolean') {
        window.postMessage({
          __gh_ext: true,
          dir: 'to-page',
          kind: 'COMPACT_UI_SYNC',
          enabled: changes[STORAGE_KEY_COMPACT].newValue
        }, window.location.origin);
      }
    });
  }

  if (chrome.runtime.onMessage) {
    chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
      if (!msg || typeof msg !== 'object') return false;
      if (msg.kind === 'GH_SET_LANG' && msg.lang) {
        pushStateToPage(msg.lang).then(() => sendResponse({ ok: true }));
        return true;
      }
      if (msg.kind === 'GH_SET_COMPACT_UI' && typeof msg.enabled === 'boolean') {
        window.postMessage({
          __gh_ext: true,
          dir: 'to-page',
          kind: 'COMPACT_UI_SYNC',
          enabled: msg.enabled
        }, window.location.origin);
        sendResponse({ ok: true });
        return false;
      }
      return false;
    });
  }

  window.addEventListener('message', (e) => {
    if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-isolated') {
      return;
    }
    if (typeof chrome === 'undefined' || !chrome.runtime || !chrome.runtime.id) {
      return;
    }
    if (e.data.kind === 'I18N_REQUEST') {
      pushStateToPage(cachedLang);
    } else if (e.data.kind === 'I18N_SET_LANG' && storedLang(e.data.lang)) {
      const norm = storedLang(e.data.lang);
      storageSet({ [STORAGE_KEY_LANG]: norm }).then(() => pushStateToPage(norm));
    } else if (e.data.kind === 'COMPACT_UI_SET' && typeof e.data.enabled === 'boolean') {
      storageSet({ [STORAGE_KEY_COMPACT]: e.data.enabled });
    }
  });

  pushStateToPage(null);
  setTimeout(() => pushStateToPage(cachedLang), 450);
})();
