// Real-browser smoke test for the HoYoLAB extension (no npm dependencies; Node >= 22).
//
// Loads extension/ unpacked into a throwaway headless Chromium profile, opens the HoYoLAB
// Interactive Map and checks, in the real page MAIN world:
//   * the current SCRIPT_BUILD booted and window.GHI18n exists (a content-script file listed in
//     two manifest entries is injected only once per frame — that left GHI18n undefined);
//   * language precedence: user choice (chrome.storage gh_lang) > desktop app language >
//     browser language; a stale page localStorage value never wins;
//   * the floating re-center button follows the language, also when switched while visible.
// The desktop app is mocked inside the service worker (CDP Fetch on http://127.0.0.1:8765), so
// the test never talks to a running GenshinHelper.
//
// Node simulations of i18n.js cannot catch Chromium injection rules; run this after any change
// to extension/manifest.json, i18n*.js, content_bridge.js, background.js or page_main.js:
//   node tests/extension_browser_smoke.mjs
// Browser: GH_CHROME=<chrome.exe>, else Playwright's Chromium under %LOCALAPPDATA%\ms-playwright.
// Needs network access to act.hoyolab.com. Exit code 0 = pass, 1 = fail, 2 = skipped.
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXT_DIR = join(ROOT, 'extension');
const EXT_ID = 'nfkijdoblmphmdljfogooajhbldkeojh';
const MAP_URL = 'https://act.hoyolab.com/ys/app/interactive-map/index.html';
const APP_ORIGIN = 'http://127.0.0.1:8765';
const BOOT_TIMEOUT_MS = 60000;
// Hang guards: every CDP call / DevTools HTTP request gets its own deadline, and the whole run
// has a hard watchdog, so a stalled page or browser fails the check instead of blocking the gate.
const CDP_CALL_TIMEOUT_MS = 15000;
const HTTP_TIMEOUT_MS = 10000;
const WATCHDOG_MS = Number(process.env.GH_SMOKE_WATCHDOG_MS) || 300000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let abortRun = null;

const SCRIPT_BUILD = (readFileSync(join(EXT_DIR, 'page_main.js'), 'utf8').match(/const SCRIPT_BUILD = '([^']+)'/) || [])[1];

function findChrome() {
  if (process.env.GH_CHROME && existsSync(process.env.GH_CHROME)) return process.env.GH_CHROME;
  const base = join(process.env.LOCALAPPDATA || '', 'ms-playwright');
  if (!existsSync(base)) return null;
  const dirs = readdirSync(base).filter((d) => /^chromium-\d+$/.test(d)).sort().reverse();
  for (const d of dirs) {
    for (const sub of ['chrome-win64', 'chrome-win']) {
      const exe = join(base, d, sub, 'chrome.exe');
      if (existsSync(exe)) return exe;
    }
  }
  return null;
}

class Cdp {
  constructor(wsUrl) {
    this.ws = new WebSocket(wsUrl);
    this.id = 0;
    this.pending = new Map();
    this.handlers = [];
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id && this.pending.has(m.id)) {
        const p = this.pending.get(m.id);
        this.pending.delete(m.id);
        clearTimeout(p.timer);
        p.resolve(m);
      } else if (m.method) {
        for (const h of this.handlers) h(m);
      }
    };
    const failAll = (why) => {
      for (const [id, p] of this.pending) {
        clearTimeout(p.timer);
        p.reject(new Error(`CDP ${p.method} #${id}: ${why}`));
      }
      this.pending.clear();
    };
    this.ws.onclose = () => failAll('socket closed');
    this.ready = new Promise((r, j) => {
      const timer = setTimeout(() => j(new Error(`CDP socket did not open within ${CDP_CALL_TIMEOUT_MS} ms`)), CDP_CALL_TIMEOUT_MS);
      this.ws.onopen = () => { clearTimeout(timer); r(); };
      this.ws.onerror = () => { clearTimeout(timer); j(new Error('CDP socket error')); };
    });
  }
  // sessionId targets a session attached with Target.setAutoAttach({ flatten: true }).
  send(method, params = {}, timeoutMs = CDP_CALL_TIMEOUT_MS, sessionId = undefined) {
    return new Promise((resolveSend, rejectSend) => {
      const id = ++this.id;
      const timer = setTimeout(() => {
        if (!this.pending.delete(id)) return;
        rejectSend(new Error(`CDP ${method} #${id} timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve: resolveSend, reject: rejectSend, timer, method });
      try {
        this.ws.send(JSON.stringify(sessionId ? { id, method, params, sessionId } : { id, method, params }));
      } catch (e) {
        this.pending.delete(id);
        clearTimeout(timer);
        rejectSend(e);
      }
    });
  }
  async eval(expression) {
    const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (r.result && r.result.exceptionDetails) throw new Error(r.result.exceptionDetails.text);
    return r.result && r.result.result ? r.result.result.value : undefined;
  }
  close() { try { this.ws.close(); } catch (e) {} }
}

const results = [];
function check(name, cond, detail) {
  results.push({ name, ok: Boolean(cond), detail });
  console.log(`  [${cond ? 'OK' : 'FAIL'}] ${name}${cond ? '' : `  -> ${JSON.stringify(detail)}`}`);
}

async function main() {
  const chrome = findChrome();
  if (!chrome) {
    console.log('[SKIP] no Chromium found (set GH_CHROME or install Playwright Chromium)');
    return 2;
  }
  const profile = mkdtempSync(join(tmpdir(), 'gh_ext_smoke_'));
  const proc = spawn(chrome, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
    `--disable-extensions-except=${EXT_DIR}`, `--load-extension=${EXT_DIR}`,
    '--no-first-run', '--no-default-browser-check', '--lang=en-US', 'about:blank',
  ], { stdio: 'ignore' });
  const sessions = [];
  const killBrowserTree = () => {
    if (process.platform === 'win32' && proc.pid) {
      spawnSync('taskkill', ['/PID', String(proc.pid), '/T', '/F'], { stdio: 'ignore' });
    } else {
      try { proc.kill(); } catch (e) {}
    }
  };
  abortRun = () => {
    for (const c of sessions) c.close();
    killBrowserTree();
    try { rmSync(profile, { recursive: true, force: true }); } catch (e) {}
  };
  try {
    let port = null;
    for (let i = 0; i < 150 && !port; i++) {
      await sleep(200);
      const f = join(profile, 'DevToolsActivePort');
      if (existsSync(f)) port = readFileSync(f, 'utf8').split('\n')[0].trim();
    }
    if (!port) throw new Error('Chromium did not expose a DevTools port');
    const http = (p, method = 'GET') => fetch(`http://127.0.0.1:${port}${p}`, { method, signal: AbortSignal.timeout(HTTP_TIMEOUT_MS) }).then((r) => r.json());

    // Mock desktop app (language vi) inside the extension service worker. Chromium stops an idle
    // MV3 service worker even with a DevTools session attached (seen in ~1 of 4 runs right after
    // install), and a session on a stopped worker never answers. So the mock does not hold one
    // worker session: a browser-level auto-attach pauses every (re)started extension worker,
    // enables Fetch interception on it, then lets it run, so no app request ever escapes the mock.
    const browserTarget = await http('/json/version');
    const browser = new Cdp(browserTarget.webSocketDebuggerUrl);
    sessions.push(browser);
    await browser.ready;
    const swSessions = new Set();
    let swStarts = 0;
    let swReadyCount = 0;
    let swCrashedCount = 0;
    let swTargetId = null;
    const waitUntil = async (pred, timeoutMs = 15000, intervalMs = 100) => {
      const deadline = Date.now() + timeoutMs;
      let lastVal = null;
      while (Date.now() < deadline) {
        try {
          lastVal = await pred();
          if (lastVal) return lastVal;
        } catch (e) {
          lastVal = null;
        }
        await sleep(Math.min(intervalMs, Math.max(10, deadline - Date.now())));
      }
      return lastVal;
    };
    browser.handlers.push((m) => {
      if (m.method === 'Target.attachedToTarget') {
        const { sessionId, targetInfo, waitingForDebugger } = m.params;
        const isExtWorker = targetInfo.type === 'service_worker' && targetInfo.url.includes(EXT_ID);
        (async () => {
          if (!isExtWorker) {
            if (waitingForDebugger) {
              browser.send('Runtime.runIfWaitingForDebugger', {}, 3000, sessionId).catch(() => {});
            }
            browser.send('Target.detachFromTarget', { sessionId }, 3000).catch(() => {});
            return;
          }
          swSessions.add(sessionId);
          swTargetId = targetInfo.targetId;
          swStarts++;
          await browser.send('Fetch.enable', { patterns: [{ urlPattern: `${APP_ORIGIN}/*` }] }, CDP_CALL_TIMEOUT_MS, sessionId);
          if (waitingForDebugger) await browser.send('Runtime.runIfWaitingForDebugger', {}, CDP_CALL_TIMEOUT_MS, sessionId);
          swReadyCount++;
        })().catch((e) => console.log(`[warn] auto-attach ${targetInfo.type} ${targetInfo.url}: ${e.message}`));
      } else if (m.method === 'Inspector.targetCrashed' && swSessions.has(m.sessionId)) {
        swCrashedCount++;
      } else if (m.method === 'Inspector.targetReloadedAfterCrash' && swSessions.has(m.sessionId)) {
        (async () => {
          swStarts++;
          await browser.send('Fetch.enable', { patterns: [{ urlPattern: `${APP_ORIGIN}/*` }] }, CDP_CALL_TIMEOUT_MS, m.sessionId);
          await browser.send('Runtime.runIfWaitingForDebugger', {}, CDP_CALL_TIMEOUT_MS, m.sessionId);
          swReadyCount++;
        })().catch((e) => console.log(`[warn] targetReloadedAfterCrash: ${e.message}`));
      } else if (m.method === 'Target.detachedFromTarget') {
        swSessions.delete(m.params.sessionId);
      } else if (m.method === 'Fetch.requestPaused' && swSessions.has(m.sessionId)) {
        const path = new URL(m.params.request.url).pathname;
        const body = path === '/status'
          ? { ok: true, language: 'vi', engine_active: true, latest_seq: 0 }
          : path === '/poll' ? { events: [], latest_seq: 0 } : { ok: true };
        browser.send('Fetch.fulfillRequest', {
          requestId: m.params.requestId,
          responseCode: 200,
          responseHeaders: [{ name: 'Content-Type', value: 'application/json' }, { name: 'Access-Control-Allow-Origin', value: '*' }],
          body: Buffer.from(JSON.stringify(body)).toString('base64'),
        }, CDP_CALL_TIMEOUT_MS, m.sessionId).catch(() => {});
      }
    });
    await browser.send('Target.setAutoAttach', {
      autoAttach: true, waitForDebuggerOnStart: true, flatten: true,
      filter: [{ type: 'service_worker', exclude: false }, { exclude: true }],
    });

    // The user's language choice lives in chrome.storage. Write it from an extension-origin page,
    // which has chrome.storage whether or not the service worker is running right now.
    const extTarget = await http(`/json/new?chrome-extension://${EXT_ID}/manifest.json`, 'PUT');
    const ext = new Cdp(extTarget.webSocketDebuggerUrl);
    sessions.push(ext);
    await ext.ready;
    let extReady = null;
    for (let i = 0; i < 50 && extReady !== 'object'; i++) {
      extReady = await ext.eval('typeof (window.chrome && chrome.storage)').catch(() => null);
      if (extReady !== 'object') await sleep(200);
    }
    if (extReady !== 'object') throw new Error('chrome.storage not available on the extension page');
    const getBgState = (extClient = ext) => extClient.eval(`new Promise((r) => chrome.runtime.sendMessage({ kind: 'GET_STATE' }, r))`);
    if (swReadyCount === 0) {
      await waitUntil(() => swReadyCount > 0, 3000, 100);
    }
    if (swReadyCount === 0) {
      await getBgState(ext).catch(() => null);
      await waitUntil(() => swReadyCount > 0, 10000, 100);
    }
    if (swReadyCount === 0) throw new Error('extension service worker not found (extension failed to load?)');
    const swVersions = new Map();
    ext.handlers.push((m) => {
      if (m.method === 'ServiceWorker.workerVersionUpdated' && Array.isArray(m.params && m.params.versions)) {
        for (const v of m.params.versions) {
          if (v && v.versionId && String(v.scriptURL || '').includes(EXT_ID)) {
            swVersions.set(v.versionId, v);
          }
        }
      }
    });
    await ext.send('ServiceWorker.enable');
    const findActiveSwVersion = () => {
      for (const v of swVersions.values()) {
        if (v.status === 'activated' && v.runningStatus === 'running') return v;
      }
      for (const v of swVersions.values()) {
        if (v.runningStatus === 'running' || v.runningStatus === 'starting') return v;
      }
      return null;
    };
    const stopExtServiceWorker = async () => {
      let activeVer = findActiveSwVersion();
      if (!swTargetId || !activeVer || activeVer.runningStatus !== 'running') {
        await getBgState(ext).catch(() => null);
        activeVer = await waitUntil(() => (swTargetId ? findActiveSwVersion() : null), 10000, 50);
      }
      if (!swTargetId || !activeVer) throw new Error('No active extension service worker found to stop');
      const crashedBefore = swCrashedCount;
      await browser.send('Target.closeTarget', { targetId: swTargetId });
      const stopped = await waitUntil(() => {
        const cur = swVersions.get(activeVer.versionId);
        return (swCrashedCount > crashedBefore || swSessions.size === 0) && (!cur || cur.runningStatus === 'stopped');
      }, 10000, 50);
      if (!stopped) throw new Error(`ServiceWorker ${activeVer.versionId} did not stop within timeout`);
    };
    const setChoice = (lang) => ext.eval(`new Promise((r) => chrome.storage.local.${lang ? `set({ gh_lang: '${lang}' }` : `remove('gh_lang'`}, () => r(true)))`);

    const pageTarget = await http('/json/new?about:blank', 'PUT');
    const page = new Cdp(pageTarget.webSocketDebuggerUrl);
    sessions.push(page);
    await page.ready;
    await page.send('Page.enable');
    await page.send('Runtime.enable');
    // A stale value under the pre-r6 key, as left by older builds (browser default marked explicit).
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: "try { localStorage.setItem('gh_lang', 'en'); } catch (e) {}" });

    const state = `({
      build: window.__gh_booted_build || null,
      i18n: typeof window.GHI18n,
      lang: window.GHI18n ? GHI18n.getLang() : null,
      explicit: window.GHI18n ? GHI18n.hasExplicitPreference() : null,
      focus: window.GHI18n ? GHI18n.t('page.route.focus_btn') : null,
      recenter: (document.getElementById('gh-recenter-btn') || {}).textContent || null
    })`;
    const waitFor = async (pred, timeoutMs = BOOT_TIMEOUT_MS) => {
      const t0 = Date.now();
      let s = null;
      let lastErr = null;
      while (Date.now() - t0 < timeoutMs) {
        try {
          s = await page.eval(state);
          lastErr = null;
        } catch (e) {
          s = null;
          lastErr = e && e.message ? e.message : String(e);
        }
        if (s && pred(s)) return s;
        await sleep(300);
      }
      return s || (lastErr ? { evalError: lastErr } : null);
    };
    const reload = async () => {
      await page.send('Page.reload', { ignoreCache: false });
    };
    const dragMap = () => page.eval(`(async () => {
      const c = document.querySelector('.leaflet-container');
      if (!c) return false;
      const r = c.getBoundingClientRect();
      const x = r.left + r.width / 2, y = r.top + r.height / 2;
      c.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, clientX: x, clientY: y, button: 0 }));
      await new Promise((res) => setTimeout(res, 160));
      c.dispatchEvent(new MouseEvent('mousemove', { bubbles: true, clientX: x + 60, clientY: y + 10, button: 0 }));
      window.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
      await new Promise((res) => setTimeout(res, 300));
      return true;
    })()`);
    // The map (and its drag-unpin listeners) may still be loading: retry until the button shows.
    const dragUntilRecenter = async (timeoutMs = 30000) => {
      const t0 = Date.now();
      let s = null;
      let syncedMarker = false;
      let dragTries = 0;
      while (Date.now() - t0 < timeoutMs) {
        const dragged = await dragMap().catch(() => false);
        s = await page.eval(state).catch(() => null);
        if (s && s.recenter) return s;
        if (dragged) {
          dragTries += 1;
          if (!syncedMarker && dragTries >= 2) {
            syncedMarker = await page.eval(`(() => {
              const b = window.__GH_BRIDGE__;
              if (!b || typeof b.getContext !== 'function' || typeof b.updatePlayerMarker !== 'function') return false;
              const ctx = b.getContext();
              if (!ctx || !ctx.map) return false;
              return Boolean(b.updatePlayerMarker({ lat: 0, lng: 0, map_id: 2 }));
            })()`).catch(() => false);
          }
        }
        await sleep(300);
      }
      return s;
    };

    console.log(`[1] no user choice, app = vi, browser = en, stale page localStorage gh_lang = en (build ${SCRIPT_BUILD})`);
    await setChoice(null);
    await page.send('Page.navigate', { url: MAP_URL });
    let s = await waitFor((x) => x.build === SCRIPT_BUILD && x.lang === 'vi');
    check('current SCRIPT_BUILD booted in the page', s && s.build === SCRIPT_BUILD, s);
    check('window.GHI18n exists in the page MAIN world', s && s.i18n === 'object', s);
    check('desktop app language applies (stale page value ignored)', s && s.lang === 'vi' && s.explicit === false, s);
    check('page labels translated (vi)', s && s.focus === 'Xem tuyến', s);
    s = await dragUntilRecenter();
    check('re-center button follows vi', s && s.recenter === 'Bám lại vị trí', s);

    console.log('[2] user picked EN in the popup (chrome.storage gh_lang = en), app = vi');
    await setChoice('en');
    await reload();
    s = await waitFor((x) => x.build === SCRIPT_BUILD && x.explicit === true);
    check('user choice beats the app language', s && s.lang === 'en' && s.focus === 'View Route', s);
    s = await dragUntilRecenter();
    check('re-center button follows en', s && s.recenter === 'Re-center', s);

    console.log('[3] user switches to VI while the re-center button is visible');
    await setChoice('vi');
    s = await waitFor((x) => x.lang === 'vi' && x.recenter === 'Bám lại vị trí', 8000);
    check('live switch re-renders the re-center button', s && s.lang === 'vi' && s.recenter === 'Bám lại vị trí', s);

    console.log('[4] MV3 service worker restart does not re-inject live open map tabs');
    const AS_URL = 'https://genshin-impact-map.appsample.com/';
    const asTarget = await http('/json/new?about:blank', 'PUT');
    const asPage = new Cdp(asTarget.webSocketDebuggerUrl);
    sessions.push(asPage);
    await asPage.ready;
    await asPage.send('Page.enable');
    await asPage.send('Runtime.enable');
    asPage.handlers.push((m) => {
      if (m.method !== 'Fetch.requestPaused') return;
      const reqUrl = m.params.request.url;
      if (reqUrl.startsWith(AS_URL)) {
        const isDoc = m.params.resourceType === 'Document' || reqUrl === AS_URL;
        asPage.send('Fetch.fulfillRequest', {
          requestId: m.params.requestId,
          responseCode: isDoc ? 200 : 204,
          responseHeaders: isDoc ? [{ name: 'Content-Type', value: 'text/html; charset=utf-8' }] : [],
          body: isDoc ? Buffer.from('<!doctype html><html><head><title>AppSample Fixture</title></head><body><div id="root"></div></body></html>').toString('base64') : ''
        }).catch(() => {});
      } else {
        asPage.send('Fetch.continueRequest', { requestId: m.params.requestId }).catch(() => {});
      }
    });
    await asPage.send('Fetch.enable', { patterns: [{ urlPattern: `${AS_URL}*` }] });
    await asPage.send('Page.navigate', { url: AS_URL });

    const asInitial = await waitUntil(async () => {
      const v = await asPage.eval(`(() => {
        const b = window.__GH_BRIDGE__;
        if (!b || typeof b.tabId !== 'string' || typeof window.__gh_provider_teardown !== 'function') return null;
        if (typeof window.GHI18n !== 'object' || typeof window.GHCoordTransform !== 'object') return null;
        return { tabId: b.tabId, providerId: b.providerId };
      })()`);
      return v && v.tabId ? v : null;
    }, 15000, 100);
    check('AppSample fixture booted provider_runtime with __GH_BRIDGE__.tabId', Boolean(asInitial && asInitial.tabId && asInitial.providerId === 'appsample'), asInitial);

    const tabsBefore = await waitUntil(async () => {
      const st = await getBgState(ext);
      return st && st.tabCount >= 2 ? st : null;
    }, 10000, 100);
    check('both HoYoLAB and AppSample tabs connected before SW stop', Boolean(tabsBefore && tabsBefore.tabCount >= 2), tabsBefore);

    const chromeTabsInfo = await ext.eval(`new Promise((r) => chrome.tabs.query({}, (ts) => r(ts.map((t) => ({ id: t.id, url: t.url })))))`);
    const asChromeTab = Array.isArray(chromeTabsInfo) ? chromeTabsInfo.find((t) => t.url && t.url.startsWith(AS_URL)) : null;
    const hlChromeTab = Array.isArray(chromeTabsInfo) ? chromeTabsInfo.find((t) => t.url && t.url.includes('hoyolab.com/ys/app/interactive-map/')) : null;
    if (!asChromeTab || !hlChromeTab) throw new Error(`Missing Chrome tab ids: ${JSON.stringify(chromeTabsInfo)}`);

    await asPage.eval(`(() => {
      window.__gh_smoke_teardown_count = 0;
      const origTeardown = window.__gh_provider_teardown;
      window.__gh_provider_teardown = function () {
        window.__gh_smoke_teardown_count = (window.__gh_smoke_teardown_count || 0) + 1;
        return origTeardown.apply(this, arguments);
      };
      window.__GH_BRIDGE__.__smokeToken = 'initial';
      window.GHI18n.__smokeToken = 'initial';
      window.GHCoordTransform.__smokeToken = 'initial';
      return true;
    })()`);
    await page.eval(`(() => {
      window.GHI18n.__smokeToken = 'initial';
      window.GHCoordTransform.__smokeToken = 'initial';
      return true;
    })()`);

    const swBeforeStop = swReadyCount;
    await stopExtServiceWorker();
    const wakeStatePromise = getBgState(ext);
    const swRestarted = await waitUntil(() => swReadyCount > swBeforeStop, 15000, 50);
    check('MV3 service worker stopped and restarted', Boolean(swRestarted), { swBeforeStop, swReadyCount });
    await wakeStatePromise;

    // 2-hop FIFO barrier (ISOLATED then MAIN) on both map tabs so any queued startup injection has run.
    await ext.eval(`(async () => {
      for (const tabId of [${asChromeTab.id}, ${hlChromeTab.id}]) {
        await chrome.scripting.executeScript({ target: { tabId }, func: () => true });
        await chrome.scripting.executeScript({ target: { tabId }, world: 'MAIN', func: () => true });
      }
      return true;
    })()`);

    const tabsAfterWake = await waitUntil(async () => {
      const st = await getBgState(ext);
      return st && st.tabCount >= 2 ? st : null;
    }, 3000, 100);
    check('live bridges reconnected promptly on SW restart', Boolean(tabsAfterWake && tabsAfterWake.tabCount >= 2), tabsAfterWake);

    const asAfterRestart = await asPage.eval(`({
      tabId: window.__GH_BRIDGE__ ? window.__GH_BRIDGE__.tabId : null,
      bridgeToken: window.__GH_BRIDGE__ ? window.__GH_BRIDGE__.__smokeToken : null,
      teardownCount: window.__gh_smoke_teardown_count,
      i18nToken: window.GHI18n ? window.GHI18n.__smokeToken : null,
      ctToken: window.GHCoordTransform ? window.GHCoordTransform.__smokeToken : null
    })`);
    check(
      'AppSample tabId unchanged and MAIN scripts not re-executed after SW restart',
      Boolean(
        asAfterRestart &&
        asAfterRestart.tabId === asInitial.tabId &&
        asAfterRestart.bridgeToken === 'initial' &&
        asAfterRestart.teardownCount === 0 &&
        asAfterRestart.i18nToken === 'initial' &&
        asAfterRestart.ctToken === 'initial'
      ),
      { before: asInitial, after: asAfterRestart }
    );

    const hlAfterRestart = await page.eval(`({
      i18nToken: window.GHI18n ? window.GHI18n.__smokeToken : null,
      ctToken: window.GHCoordTransform ? window.GHCoordTransform.__smokeToken : null
    })`);
    check(
      'HoYoLAB MAIN i18n/coord_transform instances preserved across SW restart',
      Boolean(hlAfterRestart && hlAfterRestart.i18nToken === 'initial' && hlAfterRestart.ctToken === 'initial'),
      hlAfterRestart
    );

    await setChoice('en');
    const recenterAfterRestart = await waitFor((x) => x.lang === 'en' && x.recenter === 'Re-center', 8000);
    check(
      'HoYoLAB live language switch still re-renders re-center button after SW restart',
      Boolean(recenterAfterRestart && recenterAfterRestart.lang === 'en' && recenterAfterRestart.recenter === 'Re-center'),
      recenterAfterRestart
    );

    console.log('[5] non-alive bridge tab still gets injected on SW restart and RECONNECT_MAP_TABS still forces');
    await ext.eval(`chrome.scripting.executeScript({
      target: { tabId: ${asChromeTab.id} },
      func: () => {
        window.__gh_bridge_alive = false;
        delete window.__gh_bridge_runtime_id;
        delete window.__gh_bridge_loaded;
        delete window.__gh_bridge_reconnect_now;
        return true;
      }
    })`);
    await asPage.eval('delete window.__GH_BRIDGE__; true');

    const swBeforeDead = swReadyCount;
    await stopExtServiceWorker();
    const wakeDeadPromise = getBgState(ext);
    const swRestartedForDead = await waitUntil(() => swReadyCount > swBeforeDead, 15000, 50);
    check('SW stopped and restarted for dead-bridge check', Boolean(swRestartedForDead), { swBeforeDead, swReadyCount });
    await wakeDeadPromise;

    const asAfterDead = await waitUntil(async () => {
      const v = await asPage.eval(`(() => {
        const b = window.__GH_BRIDGE__;
        if (!b || typeof b.tabId !== 'string') return null;
        return { tabId: b.tabId, teardownCount: window.__gh_smoke_teardown_count || 0 };
      })()`);
      return v;
    }, 10000, 100);
    check(
      'non-alive bridge tab was re-injected on SW restart',
      Boolean(asAfterDead && asAfterDead.tabId !== asInitial.tabId && asAfterDead.teardownCount >= 1),
      { before: asInitial, afterDead: asAfterDead }
    );

    const asBridgeAliveAgain = await ext.eval(`chrome.scripting.executeScript({
      target: { tabId: ${asChromeTab.id} },
      func: () => Boolean(window.__gh_bridge_alive && window.__gh_bridge_runtime_id === chrome.runtime.id)
    }).then((res) => Boolean(res && res[0] && res[0].result))`);
    check('re-injected tab restored live isolated bridge', asBridgeAliveAgain === true, asBridgeAliveAgain);

    await ext.eval(`(async () => {
      await chrome.scripting.executeScript({ target: { tabId: ${hlChromeTab.id} }, func: () => true });
      await chrome.scripting.executeScript({ target: { tabId: ${hlChromeTab.id} }, world: 'MAIN', func: () => true });
      return true;
    })()`);
    const hlAfterDead = await page.eval(`({
      i18nToken: window.GHI18n ? window.GHI18n.__smokeToken : null,
      ctToken: window.GHCoordTransform ? window.GHCoordTransform.__smokeToken : null
    })`);
    check(
      'live sibling HoYoLAB tab remained untouched while dead-bridge tab injected',
      Boolean(hlAfterDead && hlAfterDead.i18nToken === 'initial' && hlAfterDead.ctToken === 'initial'),
      hlAfterDead
    );

    const tabsAfterDead = await waitUntil(async () => {
      const st = await getBgState(ext);
      return st && st.tabCount >= 2 ? st : null;
    }, 3000, 100);
    check('both map tabs connected after dead-bridge re-injection', Boolean(tabsAfterDead && tabsAfterDead.tabCount >= 2), tabsAfterDead);

    const tabIdBeforeForce = asAfterDead ? asAfterDead.tabId : asInitial.tabId;
    const forceReconnectRes = await ext.eval(`new Promise((r) => chrome.runtime.sendMessage({ kind: 'RECONNECT_MAP_TABS' }, r))`);
    const asAfterForce = await waitUntil(async () => {
      const v = await asPage.eval(`(() => {
        const b = window.__GH_BRIDGE__;
        if (!b || !b.tabId || b.tabId === ${JSON.stringify(tabIdBeforeForce)}) return null;
        return { tabId: b.tabId, teardownCount: window.__gh_smoke_teardown_count || 0 };
      })()`);
      return v;
    }, 10000, 100);
    check(
      'RECONNECT_MAP_TABS still forces re-injection on live tab',
      Boolean(forceReconnectRes && forceReconnectRes.ok && asAfterForce && asAfterForce.tabId !== tabIdBeforeForce),
      { forceReconnectRes, tabIdBeforeForce, asAfterForce }
    );

    // The forced re-injection also re-ran i18n.js in the HoYoLAB tab while page_main.js exited
    // early (same SCRIPT_BUILD). Its GHI18n.onChange handler must follow the new GHI18n.
    const hlAfterForce = await waitUntil(async () => {
      const v = await page.eval(`(() => ({
        i18nToken: window.GHI18n ? (window.GHI18n.__smokeToken || null) : 'missing',
        listeners: window.__gh_i18n_listeners instanceof Set ? window.__gh_i18n_listeners.size : -1
      }))()`);
      return v && v.i18nToken !== 'initial' ? v : null;
    }, 10000, 100);
    check(
      'RECONNECT_MAP_TABS re-ran i18n.js in the HoYoLAB tab (new GHI18n instance)',
      Boolean(hlAfterForce && hlAfterForce.i18nToken !== 'initial' && hlAfterForce.i18nToken !== 'missing'),
      hlAfterForce
    );
    await setChoice('vi');
    const recenterAfterI18nRerun = await waitFor((x) => x.lang === 'vi' && x.recenter === 'Bám lại vị trí', 8000);
    check(
      'HoYoLAB re-center button still follows a live language switch after i18n.js re-runs',
      Boolean(recenterAfterI18nRerun && recenterAfterI18nRerun.lang === 'vi' && recenterAfterI18nRerun.recenter === 'Bám lại vị trí'),
      recenterAfterI18nRerun
    );

    // Informational: >1 means Chromium stopped and restarted the worker during the run.
    console.log(`[info] extension service worker started ${swStarts} time(s); app mock attached each time`);
  } finally {
    for (const c of sessions) c.close();
    killBrowserTree();
    await sleep(400);
    try { rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 250 }); } catch (e) {}
  }
  const failed = results.filter((r) => !r.ok);
  console.log(failed.length ? `FAILED: ${failed.length}/${results.length}` : `ALL ${results.length} EXTENSION BROWSER CHECKS PASSED`);
  return failed.length ? 1 : 0;
}

const watchdog = setTimeout(() => {
  console.error(`[ERROR] smoke test exceeded the ${WATCHDOG_MS} ms watchdog; aborting`);
  if (abortRun) abortRun();
  process.exit(1);
}, WATCHDOG_MS);
// Kept referenced on purpose: main() always ends in process.exit(), and a referenced timer stops
// Node from exiting 0 on an empty event loop while a promise is still pending.
void watchdog;

main().then((code) => process.exit(code), (err) => {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  process.exit(1);
});
