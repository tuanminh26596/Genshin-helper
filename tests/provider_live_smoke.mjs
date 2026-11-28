// Opt-in live multi-provider browser smoke & orientation test (Node >= 22, zero npm deps).
//
// Loads extension/ unpacked into a throwaway headless Chromium profile (guest session, never
// touching the signed-in HoYoLAB test profile) and mocks the desktop app (http://127.0.0.1:8765/*)
// via browser-level CDP Target.setAutoAttach + Fetch interception across MV3 SW restarts.
//
// Checks:
//   A) Live boot, /calibrate_provider traffic, and SYNC_POSITION_ONLY on AppSample & Kongying v3.
//   B) Guest mark/unmark checks: Kongying localStorage marked_layers + /action_feedback, and
//      AppSample guest sign-in error /action_feedback + #gh-mini-toast.
//   C) GATING: Synthetic 8-heading walk (0..315 deg) on AppSample, Kongying v3, and HoYoLAB,
//      measuring both step endpoints in the same map camera state and checking arrow (--gh-dir),
//      cone (--gh-rot + .gh-has-fov), computed SVG transforms, and rendered marker pixel position.
//   D) INFORMATIONAL: ~200-frame replay from scratch/qa_sessions/sess_20261127_211043/meta.jsonl
//      across AppSample, Kongying v3, and HoYoLAB reporting per-provider & cross-provider agreement.
//
// Usage:
//   node tests/provider_live_smoke.mjs
// Optional env:
//   GH_CHROME=<chrome.exe>              Chromium executable override (else Playwright Chromium)
//   GH_SMOKE_WATCHDOG_MS=<ms>           Hard watchdog timeout (default 300000)
//   GH_PROVIDER_RUNTIME_PATH=<path.js>  Override extension/provider_runtime.js (for seeded-fault checks)
//   GH_FORCE_SW_RESTART=1               Force-stop extension SW early in run to verify restart recovery
// Exit code: 0 if no unexpected failures, 1 on unexpected failure/error, 2 if Chromium not found.
import { spawn, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXT_DIR = join(ROOT, 'extension');
const EXT_ID = 'nfkijdoblmphmdljfogooajhbldkeojh';
const VECTORS_PATH = join(ROOT, 'tests', 'fixtures', 'multimap', 'transform_vectors.json');
const APP_ORIGIN = 'http://127.0.0.1:8765';

const APPSAMPLE_URL = 'https://genshin-impact-map.appsample.com/';
const KONGYING_URL = 'https://v3.yuanshen.site/';
const HOYOLAB_URL = 'https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2';

const CDP_CALL_TIMEOUT_MS = 15000;
const HTTP_TIMEOUT_MS = 10000;
const WATCHDOG_MS = Number(process.env.GH_SMOKE_WATCHDOG_MS) || 300000;
const HEADINGS = [0, 45, 90, 135, 180, 225, 270, 315];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let abortRun = null;

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

function resolveQaSessionMetaPath() {
  const rel = join('scratch', 'qa_sessions', 'sess_20261127_211043', 'meta.jsonl');
  const direct = join(ROOT, rel);
  if (existsSync(direct)) return direct;
  const out = spawnSync('git', ['worktree', 'list', '--porcelain'], { cwd: ROOT, encoding: 'utf8' });
  if (!out.error && out.status === 0 && out.stdout) {
    for (const line of out.stdout.split(/\r?\n/)) {
      if (line.startsWith('worktree ')) {
        const wtPath = line.slice('worktree '.length).trim();
        const candidate = join(wtPath, rel);
        if (existsSync(candidate)) return candidate;
      }
    }
  }
  return null;
}

function angularDiff(a, b) {
  if (!Number.isFinite(a) || !Number.isFinite(b)) return Infinity;
  let d = ((a - b) % 360 + 540) % 360 - 180;
  return Math.abs(d);
}

function normDeg(a) {
  return ((a % 360) + 360) % 360;
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
    if (r.result && r.result.exceptionDetails) {
      const desc = r.result.exceptionDetails.exception
        ? r.result.exceptionDetails.exception.description
        : r.result.exceptionDetails.text;
      throw new Error(desc || 'Runtime.evaluate exception');
    }
    return r.result && r.result.result ? r.result.result.value : undefined;
  }
  close() {
    try {
      if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING) {
        this.ws.close();
      }
    } catch {}
  }
}

function sanitizeUrlForLog(rawUrl) {
  try {
    const u = new URL(rawUrl);
    const keys = [...u.searchParams.keys()].sort();
    return `${u.origin}${u.pathname}${keys.length ? `?keys=${keys.join(',')}` : ''}`;
  } catch {
    return String(rawUrl || '').slice(0, 160);
  }
}

function sanitizeLogText(text) {
  return String(text || '')
    .replace(/\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+/gi, '$1 [REDACTED]')
    .replace(/\b(access_token|refresh_token|cookie|token|authorization)=([^&\s;]+)/gi, '$1=[REDACTED]')
    .slice(0, 240);
}

function isKongyingAppOrApiRequest(rawUrl) {
  try {
    const u = new URL(rawUrl);
    const host = u.hostname.toLowerCase();
    if (!['v3.yuanshen.site', 'yuanshen.site', 'cloud.yuanshen.site', 'assets.yuanshen.site'].includes(host)) {
      return false;
    }
    if (/\.(png|jpg|jpeg|webp|woff2?|css|svg|ico)(\?|$)/i.test(u.pathname)) {
      return false;
    }
    return true;
  } catch {
    return false;
  }
}

async function attachPageDiagnostics(page) {
  const diag = {
    t0: Date.now(),
    requests: new Map(),
    consoleErrors: [],
    priorAttemptSummaries: [],
    resetWindow(note = '') {
      const nowOffset = Date.now() - this.t0;
      const reqList = [...this.requests.values()].filter((r) => isKongyingAppOrApiRequest(r.url));
      const pending = reqList
        .filter((r) => r.tEnd === null)
        .map((r) => `${r.method} ${r.url} (${nowOffset - r.t0}ms)`)
        .slice(0, 4);
      const failed = reqList
        .filter((r) => r.err || (r.status !== null && r.status >= 400))
        .map((r) => `${r.method} ${r.url} (${r.err || r.status})`)
        .slice(0, 4);
      this.priorAttemptSummaries.push({
        note,
        elapsedMs: nowOffset,
        reqCount: reqList.length,
        pending,
        failed,
        consoleErrCount: this.consoleErrors.length
      });
      this.requests.clear();
      this.consoleErrors.length = 0;
      this.t0 = Date.now();
    }
  };
  page.handlers.push((m) => {
    const dt = Date.now() - diag.t0;
    if (m.method === 'Network.requestWillBeSent') {
      const { requestId, request } = m.params;
      if (!request || !request.url || request.url.startsWith('data:') || request.url.startsWith('blob:')) return;
      diag.requests.set(requestId, {
        url: sanitizeUrlForLog(request.url),
        method: request.method || 'GET',
        t0: dt,
        status: null,
        err: null,
        tResp: null,
        tEnd: null,
        bytes: 0
      });
    } else if (m.method === 'Network.responseReceived') {
      const r = diag.requests.get(m.params.requestId);
      if (r && m.params.response) {
        r.status = m.params.response.status;
        r.tResp = dt;
      }
    } else if (m.method === 'Network.loadingFinished') {
      const r = diag.requests.get(m.params.requestId);
      if (r) {
        r.tEnd = dt;
        r.bytes = m.params.encodedDataLength || 0;
      }
    } else if (m.method === 'Network.loadingFailed') {
      const r = diag.requests.get(m.params.requestId);
      if (r) {
        r.err = m.params.errorText || 'failed';
        r.tEnd = dt;
      }
    } else if (m.method === 'Runtime.consoleAPICalled' && (m.params.type === 'error' || m.params.type === 'warning')) {
      if (diag.consoleErrors.length < 30) {
        const txt = (m.params.args || []).map((a) => a.value ?? a.description ?? '').join(' ');
        diag.consoleErrors.push(`[${dt}ms ${m.params.type}] ${sanitizeLogText(txt)}`);
      }
    } else if (m.method === 'Runtime.exceptionThrown') {
      if (diag.consoleErrors.length < 30) {
        const ex = m.params.exceptionDetails || {};
        const txt = (ex.exception && ex.exception.description) || ex.text || 'exception';
        diag.consoleErrors.push(`[${dt}ms EXC] ${sanitizeLogText(txt)}`);
      }
    }
  });
  await page.send('Page.enable');
  await page.send('Network.enable');
  return diag;
}

function classifyKongyingSiteIssue(diag, { pendingThresholdMs = 8000 } = {}) {
  if (!diag) return { unavailable: false, summary: '', issues: [] };
  const nowOffset = Date.now() - diag.t0;
  const relevant = [...diag.requests.values()].filter((r) => isKongyingAppOrApiRequest(r.url));
  const issues = [];
  if (relevant.length === 0) {
    issues.push('no Kongying HTTP requests observed');
  }
  const docReq = relevant.find((r) => r.url === 'https://v3.yuanshen.site/' || r.url === 'https://v3.yuanshen.site');
  if (docReq && docReq.tEnd === null && !docReq.err) {
    issues.push(`document GET ${docReq.url} never completed (${nowOffset - docReq.t0}ms)`);
  }
  const oauthReqs = relevant.filter((r) => r.url.includes('/oauth/token'));
  if (oauthReqs.length > 0 && !oauthReqs.some((r) => r.method === 'POST' && r.status === 200 && r.tEnd !== null)) {
    issues.push(`guest /oauth/token did not succeed (${nowOffset - oauthReqs[0].t0}ms)`);
  }
  for (const r of relevant) {
    if (r.err) {
      issues.push(`${r.method} ${r.url} err=${r.err}`);
    } else if (r.status !== null && (r.status >= 500 || r.status === 429 || r.status === 401 || r.status === 403)) {
      issues.push(`${r.method} ${r.url} status=${r.status}`);
    } else if (r.tEnd === null && (nowOffset - r.t0) >= pendingThresholdMs) {
      issues.push(`${r.method} ${r.url} pending=${nowOffset - r.t0}ms`);
    }
  }
  const deduped = [...new Set(issues)];
  return {
    unavailable: deduped.length > 0,
    summary: deduped.slice(0, 4).join('; '),
    issues: deduped
  };
}

const KONGYING_FIND_ITEM_SELECTOR_JS = `function findItemSelector(vm) {
  if (!vm || !vm.$) return null;
  const seen = new Set();
  const walk = (vnode) => {
    if (!vnode || typeof vnode !== 'object' || seen.has(vnode)) return null;
    seen.add(vnode);
    if (vnode.component) {
      const comp = vnode.component;
      if (comp.type && comp.type.name === 'ItemSelector' && comp.proxy) return comp.proxy;
      const hit = walk(comp.subTree);
      if (hit) return hit;
    }
    if (Array.isArray(vnode.children)) {
      for (const ch of vnode.children) {
        const hit = walk(ch);
        if (hit) return hit;
      }
    }
    if (vnode.ssFallback) {
      const hit = walk(vnode.ssFallback);
      if (hit) return hit;
    }
    return null;
  };
  return walk(vm.$.subTree);
}`;

// Helper expression snippets run inside the target page
function providerMapAccessorJs(provider) {
  if (provider === 'hoyolab') {
    return `(() => {
      const container = document.querySelector('.leaflet-container');
      if (!container) return null;
      if (container._leaflet_map) return container._leaflet_map;
      let el = container;
      while (el) {
        if (el.__vue__ && el.__vue__.map) return el.__vue__.map;
        el = el.parentElement;
      }
      return window.map && window.map._layers ? window.map : null;
    })()`;
  }
  return `(() => {
    const ctx = window.__GH_BRIDGE__ && window.__GH_BRIDGE__.getContext();
    return ctx ? ctx.map : null;
  })()`;
}

function canonicalToProviderLlJs(provider, lat, lng) {
  if (provider === 'hoyolab') {
    return `[${lat}, ${lng}]`;
  }
  return `window.GHCoordTransform.canonicalToLeaflet(window.__GH_BRIDGE__.getTransform(), ${lat}, ${lng})`;
}

async function measureStepInPage(page, provider, prevLat, prevLng, currLat, currLng) {
  return page.eval(`(() => {
    const map = ${providerMapAccessorJs(provider)};
    if (!map || typeof map.latLngToContainerPoint !== 'function') return { error: 'no map' };
    const llPrev = ${canonicalToProviderLlJs(provider, prevLat, prevLng)};
    const llCurr = ${canonicalToProviderLlJs(provider, currLat, currLng)};
    if (!llPrev || !llCurr) return { error: 'no transform' };
    const p1 = map.latLngToContainerPoint(llPrev);
    const p2 = map.latLngToContainerPoint(llCurr);
    const dx = p2.x - p1.x;
    const dy = p2.y - p1.y;
    const stepPx = Math.hypot(dx, dy);
    const screenAngle = ((Math.atan2(dx, -dy) * 180 / Math.PI) + 360) % 360;

    const wrap = document.querySelector('.gh-player-marker-wrap');
    if (!wrap) return { error: 'no .gh-player-marker-wrap', screenAngle, stepPx, p1, p2 };
    const rawDir = wrap.style.getPropertyValue('--gh-dir').trim();
    const rawRot = wrap.style.getPropertyValue('--gh-rot').trim();
    const arrowDeg = parseFloat(rawDir);
    const coneDeg = parseFloat(rawRot);
    const hasFov = wrap.classList.contains('gh-has-fov');

    const parseMatrixAngle = (el) => {
      if (!el) return null;
      const tr = getComputedStyle(el).transform;
      if (!tr || tr === 'none') return 0;
      const m = tr.match(/matrix\\(([^)]+)\\)/);
      if (!m) return null;
      const parts = m[1].split(',').map((s) => parseFloat(s.trim()));
      if (parts.length < 4 || !Number.isFinite(parts[0]) || !Number.isFinite(parts[1])) return null;
      return ((Math.atan2(parts[1], parts[0]) * 180 / Math.PI) + 360) % 360;
    };
    const arrowSvg = wrap.querySelector('.gh-player-arrow-svg');
    const fovSvg = wrap.querySelector('.gh-player-fov-svg');
    const computedArrowDeg = parseMatrixAngle(arrowSvg);
    const computedConeDeg = parseMatrixAngle(fovSvg);

    const containerRect = map.getContainer().getBoundingClientRect();
    const wrapRect = wrap.getBoundingClientRect();
    const markerX = (wrapRect.left + wrapRect.width / 2) - containerRect.left;
    const markerY = (wrapRect.top + wrapRect.height / 2) - containerRect.top;
    const posErrPx = Math.hypot(markerX - p2.x, markerY - p2.y);

    return {
      llPrev, llCurr, p1: { x: p1.x, y: p1.y }, p2: { x: p2.x, y: p2.y },
      stepPx, screenAngle,
      rawDir, rawRot, arrowDeg, coneDeg, hasFov,
      computedArrowDeg, computedConeDeg,
      markerX, markerY, posErrPx
    };
  })()`);
}

async function main() {
  const chrome = findChrome();
  if (!chrome) {
    console.log('[SKIP] no Chromium found (set GH_CHROME or install Playwright Chromium)');
    return 2;
  }

  const vectorsDoc = JSON.parse(readFileSync(VECTORS_PATH, 'utf8'));
  const profile = mkdtempSync(join(tmpdir(), 'gh_provider_live_'));
  let tempExtDir = null;
  let extPathToLoad = EXT_DIR;
  if (process.env.GH_PROVIDER_RUNTIME_PATH) {
    const overridePath = resolve(process.env.GH_PROVIDER_RUNTIME_PATH);
    if (!existsSync(overridePath)) {
      throw new Error(`GH_PROVIDER_RUNTIME_PATH not found: ${overridePath}`);
    }
    tempExtDir = mkdtempSync(join(tmpdir(), 'gh_provider_live_ext_'));
    cpSync(EXT_DIR, tempExtDir, { recursive: true });
    writeFileSync(join(tempExtDir, 'provider_runtime.js'), readFileSync(overridePath));
    extPathToLoad = tempExtDir;
    console.log(`[INFO] Using GH_PROVIDER_RUNTIME_PATH override: ${overridePath.split(/[\\/]/).pop()}`);
  }

  const proc = spawn(chrome, [
    '--headless=new',
    '--remote-debugging-port=0',
    `--user-data-dir=${profile}`,
    `--disable-extensions-except=${extPathToLoad}`,
    `--load-extension=${extPathToLoad}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--window-size=1920,1080',
    '--lang=en-US',
    'about:blank',
  ], { stdio: 'ignore' });
  const procExited = new Promise((r) => proc.once('exit', r));
  const sessions = [];

  const killAndClean = async () => {
    for (const c of sessions) c.close();
    if (process.platform === 'win32' && proc.pid) {
      spawnSync('taskkill', ['/PID', String(proc.pid), '/T', '/F'], { stdio: 'ignore' });
    } else if (!proc.killed && proc.exitCode === null) {
      proc.kill();
    }
    await Promise.race([procExited, sleep(2000)]);
    await sleep(300);
    for (const dir of [profile, tempExtDir]) {
      if (dir && existsSync(dir)) {
        try {
          rmSync(dir, { recursive: true, force: true, maxRetries: 15, retryDelay: 250 });
        } catch (e) {
          console.warn(`[WARN] could not remove temp dir ${dir}: ${e && e.code ? e.code : e}`);
        }
      }
    }
  };

  abortRun = () => {
    for (const c of sessions) c.close();
    if (process.platform === 'win32' && proc.pid) {
      spawnSync('taskkill', ['/PID', String(proc.pid), '/T', '/F'], { stdio: 'ignore' });
    } else if (!proc.killed && proc.exitCode === null) {
      proc.kill();
    }
    for (const dir of [profile, tempExtDir]) {
      if (!dir) continue;
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch (e) {
        console.warn(`[WARN] abort cleanup failed for ${dir}: ${e && e.code ? e.code : e}`);
      }
    }
  };

  const results = [];
  function record(name, ok, detail, knownBug = null) {
    const status = ok
      ? (knownBug ? 'XPASS' : 'OK')
      : (knownBug ? 'KNOWN-FAIL' : 'FAIL');
    results.push({ name, ok: Boolean(ok), status, detail, knownBug });
    const suffix = knownBug ? ` [known_bug: ${knownBug}]` : '';
    console.log(`  [${status}] ${name}: ${detail}${suffix}`);
  }

  try {
    const waitUntil = async (fn, timeoutMs = 45000, intervalMs = 400) => {
      const deadline = Date.now() + timeoutMs;
      let last = null;
      while (Date.now() < deadline) {
        try { last = await fn(); } catch (e) { last = null; }
        if (last) return last;
        await sleep(Math.min(intervalMs, Math.max(10, deadline - Date.now())));
      }
      return last;
    };

    const port = await waitUntil(() => {
      const f = join(profile, 'DevToolsActivePort');
      if (!existsSync(f)) return null;
      const first = readFileSync(f, 'utf8').split('\n')[0].trim();
      return first || null;
    }, 22500, 100);
    if (!port) throw new Error('Chromium did not expose a DevTools port');
    const http = (p, method = 'GET') => fetch(`http://127.0.0.1:${port}${p}`, { method, signal: AbortSignal.timeout(HTTP_TIMEOUT_MS) }).then((r) => r.json());

    // Mock desktop server state & event queue via browser-level Target.setAutoAttach so MV3
    // service-worker stops/restarts re-arm Fetch interception before the worker runs.
    const browserTarget = await http('/json/version');
    const browser = new Cdp(browserTarget.webSocketDebuggerUrl);
    sessions.push(browser);
    await browser.ready;

    const swSessions = new Set();
    let swStarts = 0;
    let swReadyCount = 0;
    let swCrashedCount = 0;
    let swTargetId = null;
    const swVersions = new Map();
    const swStateWaiters = new Set();
    const diagT0 = Date.now();
    const swLifecycleLog = [];
    const logSwEvent = (ev) => {
      if (swLifecycleLog.length >= 40) swLifecycleLog.shift();
      swLifecycleLog.push(`${Date.now() - diagT0}ms:${ev}`);
    };
    let lastPollReqAt = 0;
    let lastPollFulfillAt = 0;
    let lastActionFbAt = 0;
    let swAckTimeoutLogged = false;
    const notifySwStateWaiters = () => {
      for (const checkWaiter of [...swStateWaiters]) {
        checkWaiter();
      }
    };
    const waitForSwState = (pred, timeoutMs = 15000) => {
      try {
        if (pred()) return Promise.resolve(true);
      } catch {}
      return new Promise((resolveWait) => {
        let timer = null;
        const done = (ok) => {
          swStateWaiters.delete(onEvent);
          if (timer) clearTimeout(timer);
          resolveWait(ok);
        };
        const onEvent = () => {
          let matched = false;
          try { matched = Boolean(pred()); } catch { matched = false; }
          if (matched) done(true);
        };
        timer = setTimeout(() => {
          let matched = false;
          try { matched = Boolean(pred()); } catch { matched = false; }
          done(matched);
        }, timeoutMs);
        swStateWaiters.add(onEvent);
      });
    };

    const recorded = {
      calibrate_provider: [],
      action_feedback: [],
      nearby_pins: [],
      report_state: []
    };
    let latestSeq = 0;
    let swAckSeq = 0;
    const ackWaiters = [];
    const eventLog = [];
    let waitingPoll = null;

    const clearWaitingPollForSession = (sessionId) => {
      if (waitingPoll && waitingPoll.sessionId === sessionId) {
        clearTimeout(waitingPoll.timer);
        waitingPoll = null;
      }
    };

    const notifyAckWaiters = () => {
      for (let i = ackWaiters.length - 1; i >= 0; i--) {
        if (swAckSeq >= ackWaiters[i].targetSeq) {
          const w = ackWaiters.splice(i, 1)[0];
          clearTimeout(w.timer);
          w.resolve(true);
        }
      }
    };

    const waitForSwAck = (targetSeq, timeoutMs = 8000) => {
      if (swAckSeq >= targetSeq) return Promise.resolve(true);
      return new Promise((resolveAck) => {
        const timer = setTimeout(() => {
          const idx = ackWaiters.indexOf(entry);
          if (idx >= 0) ackWaiters.splice(idx, 1);
          if (!swAckTimeoutLogged) {
            swAckTimeoutLogged = true;
            const now = Date.now();
            console.log(`  [DIAG][sw_ack_timeout] targetSeq=${targetSeq} swAckSeq=${swAckSeq} latestSeq=${latestSeq} swStarts=${swStarts} swReady=${swReadyCount} swCrashed=${swCrashedCount} swSessions=${swSessions.size} waitingPoll=${Boolean(waitingPoll)} lastPollReqAgoMs=${lastPollReqAt ? now - lastPollReqAt : -1} lastPollFulfillAgoMs=${lastPollFulfillAt ? now - lastPollFulfillAt : -1} swLog=${JSON.stringify(swLifecycleLog.slice(-12))}`);
          }
          resolveAck(false);
        }, timeoutMs);
        const entry = { targetSeq, resolve: resolveAck, timer };
        ackWaiters.push(entry);
      });
    };

    const fulfillJson = (sessionId, requestId, payload) => {
      if (!sessionId || !swSessions.has(sessionId)) return;
      browser.send('Fetch.fulfillRequest', {
        requestId,
        responseCode: 200,
        responseHeaders: [
          { name: 'Content-Type', value: 'application/json' },
          { name: 'Access-Control-Allow-Origin', value: '*' }
        ],
        body: Buffer.from(JSON.stringify(payload), 'utf8').toString('base64')
      }, CDP_CALL_TIMEOUT_MS, sessionId).then((res) => {
        if (res && res.error && browser.ws.readyState === WebSocket.OPEN && swSessions.has(sessionId)) {
          logSwEvent(`fulfill_err:${res.error.message || res.error.code}`);
        }
      }).catch((err) => {
        if (browser.ws.readyState === WebSocket.OPEN && swSessions.has(sessionId)) {
          logSwEvent(`fulfill_rej:${err && err.message ? err.message : err}`);
          console.warn('[WARN] Fetch.fulfillRequest failed:', err && err.message ? err.message : err);
        }
      });
    };

    const pushServerEvent = (ev) => {
      const seq = ++latestSeq;
      const full = { ...ev, _seq: seq };
      eventLog.push(full);
      if (waitingPoll && seq > waitingPoll.since) {
        const w = waitingPoll;
        waitingPoll = null;
        clearTimeout(w.timer);
        fulfillJson(w.sessionId, w.requestId, full);
      }
      return seq;
    };

    const pushAndWaitDelivered = async (ev, settleMs = 40) => {
      const seq = pushServerEvent(ev);
      await waitForSwAck(seq, 8000);
      if (settleMs > 0) await sleep(settleMs);
      return seq;
    };

    const flushEventQueue = () => {
      eventLog.length = 0;
      swAckSeq = Math.max(swAckSeq, latestSeq);
      notifyAckWaiters();
      if (waitingPoll) {
        const w = waitingPoll;
        waitingPoll = null;
        clearTimeout(w.timer);
        fulfillJson(w.sessionId, w.requestId, { type: 'HEARTBEAT', _seq: latestSeq });
      }
    };

    browser.handlers.push((m) => {
      if (m.method === 'Target.attachedToTarget') {
        const { sessionId, targetInfo, waitingForDebugger } = m.params;
        const isExtWorker = targetInfo.type === 'service_worker' && String(targetInfo.url || '').includes(EXT_ID);
        (async () => {
          if (!isExtWorker) {
            if (waitingForDebugger) {
              await browser.send('Runtime.runIfWaitingForDebugger', {}, 3000, sessionId).catch(() => {});
            }
            await browser.send('Target.detachFromTarget', { sessionId }, 3000).catch(() => {});
            return;
          }
          swSessions.add(sessionId);
          swTargetId = targetInfo.targetId;
          swStarts++;
          logSwEvent(`attached:${swStarts}:wfd=${Boolean(waitingForDebugger)}`);
          const fetchRes = await browser.send('Fetch.enable', { patterns: [{ urlPattern: `${APP_ORIGIN}/*` }] }, CDP_CALL_TIMEOUT_MS, sessionId);
          if (fetchRes && fetchRes.error) {
            throw new Error(`Fetch.enable error: ${fetchRes.error.message || JSON.stringify(fetchRes.error)}`);
          }
          if (waitingForDebugger) {
            const runRes = await browser.send('Runtime.runIfWaitingForDebugger', {}, CDP_CALL_TIMEOUT_MS, sessionId);
            if (runRes && runRes.error) {
              throw new Error(`Runtime.runIfWaitingForDebugger error: ${runRes.error.message || JSON.stringify(runRes.error)}`);
            }
          }
          swReadyCount++;
          logSwEvent(`ready:${swReadyCount}`);
          notifySwStateWaiters();
        })().catch((e) => {
          swSessions.delete(sessionId);
          clearWaitingPollForSession(sessionId);
          logSwEvent(`attach_err:${e && e.message ? e.message : e}`);
          notifySwStateWaiters();
          console.warn(`[WARN] auto-attach ${targetInfo.type} ${sanitizeUrlForLog(targetInfo.url)}: ${e && e.message ? e.message : e}`);
        });
      } else if (m.method === 'Inspector.targetCrashed' && swSessions.has(m.sessionId)) {
        swCrashedCount++;
        logSwEvent(`crashed:${swCrashedCount}`);
        clearWaitingPollForSession(m.sessionId);
        notifySwStateWaiters();
      } else if (m.method === 'Inspector.targetReloadedAfterCrash' && swSessions.has(m.sessionId)) {
        (async () => {
          swStarts++;
          logSwEvent(`reloaded:${swStarts}`);
          const fetchRes = await browser.send('Fetch.enable', { patterns: [{ urlPattern: `${APP_ORIGIN}/*` }] }, CDP_CALL_TIMEOUT_MS, m.sessionId);
          if (fetchRes && fetchRes.error) {
            throw new Error(`Fetch.enable after crash error: ${fetchRes.error.message || JSON.stringify(fetchRes.error)}`);
          }
          const runRes = await browser.send('Runtime.runIfWaitingForDebugger', {}, CDP_CALL_TIMEOUT_MS, m.sessionId);
          if (runRes && runRes.error) {
            throw new Error(`Runtime.runIfWaitingForDebugger after crash error: ${runRes.error.message || JSON.stringify(runRes.error)}`);
          }
          swReadyCount++;
          logSwEvent(`ready:${swReadyCount}`);
          notifySwStateWaiters();
        })().catch((e) => {
          clearWaitingPollForSession(m.sessionId);
          logSwEvent(`reload_err:${e && e.message ? e.message : e}`);
          notifySwStateWaiters();
          console.warn(`[WARN] targetReloadedAfterCrash: ${e && e.message ? e.message : e}`);
        });
      } else if (m.method === 'Target.detachedFromTarget') {
        const sid = m.params && m.params.sessionId;
        if (swSessions.delete(sid)) {
          logSwEvent(`detached:sessLeft=${swSessions.size}`);
        }
        clearWaitingPollForSession(sid);
        notifySwStateWaiters();
      } else if (m.method === 'Fetch.requestPaused' && swSessions.has(m.sessionId)) {
        const req = m.params.request;
        const u = new URL(req.url);
        const path = u.pathname;
        let parsedBody = null;
        if (req.postData) {
          try { parsedBody = JSON.parse(req.postData); } catch (e) { parsedBody = req.postData; }
        }

        if (path === '/calibrate_provider') {
          recorded.calibrate_provider.push(parsedBody);
          fulfillJson(m.sessionId, m.params.requestId, { ok: true, transform: null });
          return;
        }
        if (path === '/action_feedback') {
          lastActionFbAt = Date.now();
          recorded.action_feedback.push(parsedBody);
          fulfillJson(m.sessionId, m.params.requestId, { ok: true });
          return;
        }
        if (path === '/nearby_pins') {
          recorded.nearby_pins.push(parsedBody);
          fulfillJson(m.sessionId, m.params.requestId, { ok: true, is_owner: true });
          return;
        }
        if (path === '/report_state') {
          recorded.report_state.push(parsedBody);
          fulfillJson(m.sessionId, m.params.requestId, { ok: true, is_owner: true });
          return;
        }
        if (path === '/status') {
          fulfillJson(m.sessionId, m.params.requestId, {
            ok: true,
            language: 'en',
            engine_active: true,
            auto_sync: true,
            auto_sync_m: true,
            minimap_tracking: true,
            latest_seq: swAckSeq
          });
          return;
        }
        if (path === '/poll') {
          lastPollReqAt = Date.now();
          const since = Number(u.searchParams.get('since'));
          if (Number.isFinite(since) && since > swAckSeq) {
            swAckSeq = since;
            while (eventLog.length > 0 && eventLog[0]._seq <= swAckSeq) {
              eventLog.shift();
            }
            notifyAckWaiters();
          }
          const effectiveSince = Number.isFinite(since) ? since : -1;
          const nextEv = eventLog.find((e) => e._seq > effectiveSince);
          if (nextEv) {
            lastPollFulfillAt = Date.now();
            fulfillJson(m.sessionId, m.params.requestId, nextEv);
            return;
          }
          // Hold briefly so pushServerEvent delivers immediately without spinning CPU
          if (waitingPoll) {
            clearTimeout(waitingPoll.timer);
            lastPollFulfillAt = Date.now();
            fulfillJson(waitingPoll.sessionId, waitingPoll.requestId, { type: 'HEARTBEAT', _seq: latestSeq });
          }
          const sessionId = m.sessionId;
          const requestId = m.params.requestId;
          const timer = setTimeout(() => {
            if (waitingPoll && waitingPoll.requestId === requestId && waitingPoll.sessionId === sessionId) {
              waitingPoll = null;
              lastPollFulfillAt = Date.now();
              fulfillJson(sessionId, requestId, { type: 'HEARTBEAT', _seq: latestSeq });
            }
          }, 250);
          waitingPoll = { requestId, sessionId, since: effectiveSince, timer };
          return;
        }
        fulfillJson(m.sessionId, m.params.requestId, { ok: true, is_owner: true });
      }
    });

    await browser.send('Target.setAutoAttach', {
      autoAttach: true,
      waitForDebuggerOnStart: true,
      flatten: true,
      filter: [{ type: 'service_worker', exclude: false }, { exclude: true }],
    });

    // Extension-origin page provides chrome.runtime/chrome.storage/ServiceWorker inspection
    // regardless of whether the MV3 service worker is currently running.
    const extTarget = await http(`/json/new?chrome-extension://${EXT_ID}/manifest.json`, 'PUT');
    const ext = new Cdp(extTarget.webSocketDebuggerUrl);
    sessions.push(ext);
    await ext.ready;
    const extReady = await waitUntil(async () => {
      const t = await ext.eval('typeof (window.chrome && chrome.storage && chrome.runtime)');
      return t === 'object' ? t : null;
    }, 10000, 100);
    if (extReady !== 'object') throw new Error('chrome.runtime not available on the extension page');

    ext.handlers.push((m) => {
      if (m.method === 'ServiceWorker.workerVersionUpdated' && Array.isArray(m.params && m.params.versions)) {
        for (const v of m.params.versions) {
          if (v && v.versionId && String(v.scriptURL || '').includes(EXT_ID)) {
            swVersions.set(v.versionId, v);
          }
        }
        notifySwStateWaiters();
      }
    });
    await ext.send('ServiceWorker.enable');

    const getBgState = () => ext.eval(`new Promise((r) => {
      try {
        chrome.runtime.sendMessage({ kind: 'GET_STATE' }, (resp) => {
          const err = chrome.runtime.lastError;
          r(err ? { error: err.message } : (resp || { error: 'empty GET_STATE response' }));
        });
      } catch (e) {
        r({ error: e && e.message ? e.message : String(e) });
      }
    })`);

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
        await getBgState().catch(() => null);
        const ready = await waitForSwState(() => Boolean(swTargetId && findActiveSwVersion()), 10000);
        activeVer = ready ? findActiveSwVersion() : null;
      }
      if (!swTargetId || !activeVer) throw new Error('No active extension service worker found to stop');
      const crashedBefore = swCrashedCount;
      await browser.send('Target.closeTarget', { targetId: swTargetId });
      const stopped = await waitForSwState(() => {
        const cur = swVersions.get(activeVer.versionId);
        return (swCrashedCount > crashedBefore || swSessions.size === 0) && (!cur || cur.runningStatus === 'stopped');
      }, 10000);
      if (!stopped) throw new Error(`ServiceWorker ${activeVer.versionId} did not stop within timeout`);
    };

    if (swReadyCount === 0) {
      await waitForSwState(() => swReadyCount > 0, 3000);
    }
    // Wake/wait for the worker and let onInstalled/onStartup reinjectOpenMapTabs settle
    // via GET_STATE while only about:blank + manifest.json are open.
    const bootBgState = await waitUntil(async () => {
      const st = await getBgState().catch(() => null);
      return swReadyCount > 0 && st && !st.error ? st : null;
    }, 12000, 100);
    if (swReadyCount === 0) throw new Error('extension service worker not found (extension failed to load?)');
    if (!bootBgState) throw new Error('extension service worker did not respond to GET_STATE');

    const openTab = async (url) => {
      const target = await http(`/json/new?${encodeURI(url)}`, 'PUT');
      const p = new Cdp(target.webSocketDebuggerUrl);
      sessions.push(p);
      await p.ready;
      await p.send('Runtime.enable');
      return { page: p, targetId: target.id };
    };

    const openTabWithDiagnostics = async (url) => {
      const target = await http('/json/new?about:blank', 'PUT');
      const p = new Cdp(target.webSocketDebuggerUrl);
      sessions.push(p);
      await p.ready;
      await p.send('Runtime.enable');
      const diag = await attachPageDiagnostics(p);
      await p.send('Page.navigate', { url });
      return { page: p, targetId: target.id, diag };
    };

    const dumpKongyingDiagnostics = async (page, diag, extra = {}) => {
      let pageState = null;
      try {
        pageState = await page.eval(`(() => {
          ${KONGYING_FIND_ITEM_SELECTOR_JS}
          const b = window.__GH_BRIDGE__;
          const ctx = b ? b.getContext() : null;
          const vm = ctx ? ctx.vm : null;
          const ms = ctx ? ctx.mainStore : null;
          const isel = vm ? findItemSelector(vm) : null;
          let totalLayers = 0;
          let dataLayers = 0;
          let leafletTeleportLayers = 0;
          let leafletPinLayers = 0;
          if (ctx && ctx.map && ctx.map._layers) {
            const seen = new Set();
            const visit = (l) => {
              if (!l || seen.has(l)) return;
              seen.add(l);
              totalLayers++;
              const d = l.options && l.options.data;
              if (d && d.id !== undefined) {
                dataLayers++;
                const title = String(d.markerTitle || '').trim();
                const isTp = Boolean(l.options && l.options.icon && l.options.icon.options && l.options.icon.options.teleport) ||
                  /^(传送锚点|七天神像|秘境|征讨领域|浪船锚点|奖励秘境)$/.test(title);
                if (isTp) leafletTeleportLayers++;
                else leafletPinLayers++;
              }
              if (typeof l.getLayers === 'function') {
                try { for (const c of l.getLayers()) visit(c); } catch {}
              } else if (typeof l.eachLayer === 'function') {
                try { l.eachLayer(visit); } catch {}
              }
            };
            for (const l of Object.values(ctx.map._layers)) visit(l);
          }
          const headers = Array.from(document.querySelectorAll('.item_selector_item .item_header'));
          const visHeaders = headers.filter((el) => el.offsetWidth > 0 && el.offsetHeight > 0).length;
          const visOptions = Array.from(document.querySelectorAll('.item_selector_item .item_body .item_option'))
            .filter((el) => el.offsetWidth > 0 && el.offsetHeight > 0).length;
          const qNotifs = Array.from(document.querySelectorAll('.q-notification__message'))
            .map((el) => String(el.textContent || '').trim().slice(0, 120))
            .filter(Boolean);
          return {
            readyState: document.readyState,
            hasBridge: Boolean(b),
            hasCtx: Boolean(ctx),
            areaCount: ms && Array.isArray(ms.area_list) ? ms.area_list.length : -1,
            selectedArea: ms ? ms.selected_area : null,
            selectedChildArea: ms && ms.selected_child_area ? {
              id: ms.selected_child_area.id,
              code: ms.selected_child_area.code,
              name: ms.selected_child_area.name
            } : null,
            teleportListLen: ms && Array.isArray(ms.teleport_list) ? ms.teleport_list.length : -1,
            selectedItemCount: ms && Array.isArray(ms.selected_item_list) ? ms.selected_item_list.length : -1,
            vm: vm ? {
              teleport_state: Boolean(vm.teleport_state),
              loading: Boolean(vm.loading),
              underground_show: Boolean(vm.underground_show),
              underground_opacity: Boolean(vm.underground_opacity),
              iconListLen: Array.isArray(vm.icon_list) ? vm.icon_list.length : -1,
              teleportMapKeys: vm.teleport_map && typeof vm.teleport_map.keys === 'function' ? [...vm.teleport_map.keys()] : [],
              teleportGroupLayers: vm.teleport_group && typeof vm.teleport_group.getLayers === 'function' ? vm.teleport_group.getLayers().length : -1,
              bxGroupLayers: vm.BXGroup && typeof vm.BXGroup.getLayers === 'function' ? vm.BXGroup.getLayers().length : -1
            } : null,
            isel: isel ? {
              item_loading: Boolean(isel.item_loading),
              typeListLen: Array.isArray(isel.type_list) ? isel.type_list.length : -1,
              itemCountTotal: Array.isArray(isel.item_list)
                ? isel.item_list.reduce((acc, arr) => acc + (Array.isArray(arr) ? arr.length : 0), 0)
                : -1
            } : null,
            leaflet: { totalLayers, dataLayers, leafletTeleportLayers, leafletPinLayers },
            bridgePins: b && typeof b.queryNearbyUncollectedPins === 'function' ? b.queryNearbyUncollectedPins().length : -1,
            dom: { headers: headers.length, visHeaders, visOptions, qNotifs }
          };
        })()`);
      } catch (e) {
        pageState = { evalError: sanitizeLogText(e && e.message ? e.message : e) };
      }
      const kyCals = recorded.calibrate_provider.filter((r) => r && r.provider === 'kongying');
      const lastCal = kyCals.length ? kyCals[kyCals.length - 1] : null;
      const nowDiag = Date.now();
      const bgStateNow = await getBgState().catch((e) => ({ evalErr: String(e) }));
      const extStats = {
        recordedCalibrateCount: kyCals.length,
        lastCalibrateAnchors: lastCal && Array.isArray(lastCal.anchors) ? lastCal.anchors.length : 0,
        swStarts,
        swReadyCount,
        swCrashedCount,
        swSessions: swSessions.size,
        swAckSeq,
        latestSeq,
        eventLogLen: eventLog.length,
        hasWaitingPoll: Boolean(waitingPoll),
        lastPollReqAgoMs: lastPollReqAt ? nowDiag - lastPollReqAt : -1,
        lastPollFulfillAgoMs: lastPollFulfillAt ? nowDiag - lastPollFulfillAt : -1,
        lastActionFbAgoMs: lastActionFbAt ? nowDiag - lastActionFbAt : -1,
        bgState: bgStateNow,
        swLog: swLifecycleLog.slice(-15)
      };
      console.log(`  [DIAG][kongying] state=${JSON.stringify(pageState)} ext=${JSON.stringify(extStats)} extra=${JSON.stringify(extra)}`);
      if (diag) {
        if (diag.priorAttemptSummaries.length) {
          console.log(`  [DIAG][kongying] priorAttempts=${JSON.stringify(diag.priorAttemptSummaries)}`);
        }
        const nowOffset = Date.now() - diag.t0;
        const reqs = [...diag.requests.values()].filter((r) => isKongyingAppOrApiRequest(r.url));
        console.log(`  [DIAG][kongying] network (${reqs.length} app/api requests, window=${nowOffset}ms):`);
        for (const r of reqs) {
          const dur = r.tEnd !== null ? `+${r.tEnd - r.t0}ms` : `PENDING(${nowOffset - r.t0}ms)`;
          console.log(`    [${r.t0}ms ${dur}] ${r.method} ${r.status ?? r.err ?? 'OPEN'} ${r.url} (${r.bytes}B)`);
        }
        if (diag.consoleErrors.length) {
          console.log(`  [DIAG][kongying] console (${diag.consoleErrors.length}):`);
          for (const line of diag.consoleErrors) {
            console.log(`    ${line}`);
          }
        }
      }
      return { pageState, extStats };
    };

    const closeTab = async (tab) => {
      tab.page.close();
      try {
        await fetch(`http://127.0.0.1:${port}/json/close/${tab.targetId}`, { signal: AbortSignal.timeout(5000) });
      } catch (e) {
        console.warn(`[WARN] closeTab(${tab.targetId}) failed:`, e && e.message ? e.message : e);
      }
    };

    // Helper: run synthetic 8-heading walk on a loaded provider tab
    const runEightHeadingWalk = async (page, providerLabel, anchorLat, anchorLng) => {
      const step = 80;
      // Anchor & zoom to clean mid-zoom so map doesn't clamp against maxBounds
      await pushAndWaitDelivered({
        type: 'SYNC_POSITION_ONLY',
        map_id: 2,
        lat: anchorLat,
        lng: anchorLng,
        heading: 0,
        camera_heading: 0,
        zoom_ratio_t: 0.65,
        has_world_coords: true,
        is_underground: false
      }, 650);

      const perHeading = [];
      for (const h of HEADINGS) {
        const rad = (h * Math.PI) / 180;
        const dlng = step * Math.sin(rad);
        const dlat = -step * Math.cos(rad);
        const midLat = anchorLat + dlat * 0.5;
        const midLng = anchorLng + dlng * 0.5;
        const endLat = anchorLat + dlat;
        const endLng = anchorLng + dlng;

        // Reset to anchor, then step mid -> end, waiting for each SW delivery ack
        await pushAndWaitDelivered({
          type: 'LIVE_POSITION',
          map_id: 2,
          lat: anchorLat,
          lng: anchorLng,
          heading: h,
          camera_heading: h,
          has_world_coords: true,
          is_underground: false
        }, 40);
        await pushAndWaitDelivered({
          type: 'LIVE_POSITION',
          map_id: 2,
          lat: midLat,
          lng: midLng,
          heading: h,
          camera_heading: h,
          has_world_coords: true,
          is_underground: false
        }, 40);
        await pushAndWaitDelivered({
          type: 'LIVE_POSITION',
          map_id: 2,
          lat: endLat,
          lng: endLng,
          heading: h,
          camera_heading: h,
          has_world_coords: true,
          is_underground: false
        }, 350);

        const m = await measureStepInPage(page, providerLabel, anchorLat, anchorLng, endLat, endLng);
        const arrowErr = angularDiff(m.screenAngle, m.arrowDeg);
        const coneErr = angularDiff(m.screenAngle, m.coneDeg);
        const compArrowErr = angularDiff(m.screenAngle, m.computedArrowDeg);
        const compConeErr = angularDiff(m.screenAngle, m.computedConeDeg);
        perHeading.push({
          h,
          screenAngle: Number(m.screenAngle?.toFixed(1)),
          arrowDeg: Number(normDeg(m.arrowDeg).toFixed(1)),
          coneDeg: Number(normDeg(m.coneDeg).toFixed(1)),
          hasFov: Boolean(m.hasFov),
          arrowErr: Number(arrowErr.toFixed(1)),
          coneErr: Number(coneErr.toFixed(1)),
          compArrowErr: Number(compArrowErr.toFixed(1)),
          compConeErr: Number(compConeErr.toFixed(1)),
          posErrPx: Number(m.posErrPx?.toFixed(2)),
          stepPx: Number(m.stepPx?.toFixed(1)),
          error: m.error || null
        });
      }

      const maxPosErr = Math.max(...perHeading.map((x) => Number.isFinite(x.posErrPx) ? x.posErrPx : 999));
      const maxArrowErr = Math.max(...perHeading.map((x) => Number.isFinite(x.arrowErr) ? x.arrowErr : 999));
      const maxConeErr = Math.max(...perHeading.map((x) => Number.isFinite(x.coneErr) ? x.coneErr : 999));
      const maxCompArrowErr = Math.max(...perHeading.map((x) => Number.isFinite(x.compArrowErr) ? x.compArrowErr : 999));
      const allFov = perHeading.every((x) => x.hasFov);
      const posOk = maxPosErr <= 8 && perHeading.every((x) => !x.error && x.stepPx > 2);
      const orientOk = maxArrowErr <= 10 && maxConeErr <= 10 && maxCompArrowErr <= 10 && allFov;
      return { perHeading, maxPosErr, maxArrowErr, maxConeErr, maxCompArrowErr, allFov, posOk, orientOk };
    };

    // Helper: run ~200-frame informational QA replay on a loaded provider tab
    const runQaReplay = async (page, providerLabel, frames) => {
      if (!frames || frames.length < 2) return null;
      // Sync to first frame cleanly
      const f0 = frames[0];
      await pushAndWaitDelivered({
        type: 'SYNC_POSITION_ONLY',
        map_id: 2,
        lat: f0.world_y,
        lng: f0.world_x,
        world_x: f0.world_x,
        world_y: f0.world_y,
        heading: f0.body ?? f0.view ?? 0,
        camera_heading: f0.camera ?? f0.view ?? 0,
        zoom_ratio_t: 0.6,
        has_world_coords: true,
        is_underground: Boolean(f0.underground)
      }, 500);

      const stepRecords = [];
      let movingSteps = 0;
      let agreeSteps = 0;
      for (let i = 1; i < frames.length; i++) {
        const prev = frames[i - 1];
        const curr = frames[i];
        const heading = Number.isFinite(curr.body) ? curr.body : (Number.isFinite(curr.view) ? curr.view : 0);
        const camHeading = Number.isFinite(curr.camera) ? curr.camera : (Number.isFinite(curr.view) ? curr.view : 0);
        const dist = Math.hypot(curr.world_x - prev.world_x, curr.world_y - prev.world_y);
        const isMovingStep = dist > 1.0 && dist <= 50;
        await pushAndWaitDelivered({
          type: 'LIVE_POSITION',
          map_id: curr.map_id,
          lat: curr.world_y,
          lng: curr.world_x,
          world_x: curr.world_x,
          world_y: curr.world_y,
          heading,
          camera_heading: camHeading,
          is_underground: Boolean(curr.underground),
          has_world_coords: true
        }, isMovingStep ? 25 : 0);
        if (isMovingStep) {
          const m = await measureStepInPage(page, providerLabel, prev.world_y, prev.world_x, curr.world_y, curr.world_x);
          if (!m.error && Number.isFinite(m.screenAngle) && Number.isFinite(m.arrowDeg)) {
            movingSteps++;
            const diff = angularDiff(m.screenAngle, m.arrowDeg);
            if (diff <= 30) agreeSteps++;
            stepRecords.push({
              idx: i,
              screenAngle: normDeg(m.screenAngle),
              arrowDeg: normDeg(m.arrowDeg),
              coneDeg: normDeg(m.coneDeg)
            });
          }
        }
      }
      return {
        provider: providerLabel,
        framesFed: frames.length,
        movingSteps,
        agreeSteps,
        agreePct: movingSteps > 0 ? Number(((agreeSteps / movingSteps) * 100).toFixed(1)) : 0,
        stepRecords
      };
    };

    // Load QA replay slice (~200 consecutive map_id=2 frames with dense walking motion)
    const qaPath = resolveQaSessionMetaPath();
    let qaFrames = null;
    if (qaPath) {
      const allEntries = readFileSync(qaPath, 'utf8')
        .trim()
        .split(/\r?\n/)
        .map((l) => JSON.parse(l))
        .filter((x) => x.map_id === 2 && Number.isFinite(x.world_x) && Number.isFinite(x.world_y));
      if (allEntries.length >= 200) {
        let bestIdx = 0;
        let bestCnt = -1;
        for (let i = 0; i <= allEntries.length - 200; i++) {
          let c = 0;
          for (let j = i + 1; j < i + 200; j++) {
            const d = Math.hypot(allEntries[j].world_x - allEntries[j - 1].world_x, allEntries[j].world_y - allEntries[j - 1].world_y);
            if (d > 1.0 && d <= 50) c++;
          }
          if (c > bestCnt) { bestCnt = c; bestIdx = i; }
        }
        qaFrames = allEntries.slice(bestIdx, bestIdx + 200);
      } else {
        qaFrames = allEntries;
      }
    }

    const qaResults = {};

    // =========================================================================
    // PROVIDER 1: AppSample (https://genshin-impact-map.appsample.com/)
    // =========================================================================
    console.log('\n[1/3] AppSample live checks');
    const asTab = await openTab(APPSAMPLE_URL);
    try {
      const asBoot = await waitUntil(async () => {
        const s = await asTab.page.eval(`(() => {
          const b = window.__GH_BRIDGE__;
          if (!b) return null;
          const ctx = b.getContext();
          if (!ctx) return null;
          const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : null;
          if (!st || !st.markersComplete) return null;
          return {
            providerId: b.providerId,
            tabId: b.tabId,
            hasRegister: typeof window.__GH_REGISTER_MAP_PROVIDER__ === 'function',
            mapId: b.getCurrentMapId(),
            pinsCount: b.queryNearbyUncollectedPins().length
          };
        })()`);
        return s;
      }, 60000, 500);
      record(
        'appsample_live_boot_and_context',
        Boolean(asBoot && asBoot.providerId === 'appsample' && asBoot.hasRegister && asBoot.mapId === 2),
        JSON.stringify(asBoot)
      );

      if (process.env.GH_FORCE_SW_RESTART === '1') {
        const swReadyBeforeStop = swReadyCount;
        await stopExtServiceWorker();
        console.log(`  [INFO] Forced SW restart: stopped extension worker (swStarts=${swStarts}, swReadyCount=${swReadyBeforeStop})`);
        let restarted = await waitForSwState(() => swReadyCount > swReadyBeforeStop, 5000);
        if (!restarted) {
          await getBgState().catch(() => null);
          restarted = await waitForSwState(() => swReadyCount > swReadyBeforeStop, 10000);
        }
        if (!restarted) {
          throw new Error(`Forced SW restart did not attach a restarted worker (swReadyCount=${swReadyCount})`);
        }
        console.log(`  [INFO] Forced SW restart: extension worker restarted and re-armed app mock (swStarts=${swStarts}, swReadyCount=${swReadyCount})`);
      }

      // Ensure calibration triggered now that markersComplete=true
      await asTab.page.eval(`window.__GH_BRIDGE__ && window.__GH_BRIDGE__.calibrateNow()`);
      const asCal = await waitUntil(() => recorded.calibrate_provider.find((r) => r && r.provider === 'appsample'), 10000, 200);
      record(
        'appsample_live_calibrate_provider',
        Boolean(asCal && asCal.provider === 'appsample' && asCal.map_id === 2 && asCal.swap_lat_lng === false && Array.isArray(asCal.anchors) && asCal.anchors.length >= 10),
        asCal ? `provider=${asCal.provider} map_id=${asCal.map_id} swap=${asCal.swap_lat_lng} anchors=${asCal.anchors.length}` : 'missing'
      );

      // SYNC_POSITION_ONLY check against transform_vectors.json (appsample_m2_mainland_mondstadt: cx=1500, cy=-2000)
      const asVec = vectorsDoc.vectors.find((v) => v.id === 'appsample_m2_mainland_mondstadt');
      await pushAndWaitDelivered({
        type: 'SYNC_POSITION_ONLY',
        map_id: 2,
        lat: asVec.canonical.y,
        lng: asVec.canonical.x,
        heading: 0,
        camera_heading: 0,
        zoom_ratio_t: 0.6,
        has_world_coords: true,
        is_underground: false
      }, 100);
      let asSyncState = null;
      await waitUntil(async () => {
        const s = await asTab.page.eval(`(() => {
          const ctx = window.__GH_BRIDGE__.getContext();
          const c = ctx.map.getCenter();
          const ll = window.GHCoordTransform.canonicalToLeaflet(window.__GH_BRIDGE__.getTransform(), ${asVec.canonical.y}, ${asVec.canonical.x});
          const pExp = ctx.map.latLngToContainerPoint(ll);
          const wrap = document.querySelector('.gh-player-marker-wrap');
          const cRect = ctx.map.getContainer().getBoundingClientRect();
          const wRect = wrap ? wrap.getBoundingClientRect() : null;
          const mErrPx = wRect ? Math.hypot((wRect.left + wRect.width / 2 - cRect.left) - pExp.x, (wRect.top + wRect.height / 2 - cRect.top) - pExp.y) : 999;
          return {
            center: { lat: c.lat, lng: c.lng },
            expected: { lat: ll[0], lng: ll[1] },
            zoom: ctx.map.getZoom(),
            animating: Boolean(ctx.map._animatingZoom),
            mErrPx
          };
        })()`);
        if (s) asSyncState = s;
        return Boolean(
          s &&
          !s.animating &&
          Math.abs(s.center.lat - asVec.expected_leaflet.lat) < 0.01 &&
          Math.abs(s.center.lng - asVec.expected_leaflet.lng) < 0.01 &&
          s.mErrPx <= 8
        );
      }, 5000, 150);
      const asSyncOk = Boolean(
        asSyncState &&
        Math.abs(asSyncState.center.lat - asVec.expected_leaflet.lat) < 0.01 &&
        Math.abs(asSyncState.center.lng - asVec.expected_leaflet.lng) < 0.01 &&
        asSyncState.mErrPx <= 8
      );
      if (!asSyncOk) {
        const asDiag = await asTab.page.eval(`(() => ({
          curTabId: window.__GH_BRIDGE__ ? window.__GH_BRIDGE__.tabId : null,
          hasWrap: Boolean(document.querySelector('.gh-player-marker-wrap')),
          url: location.href
        }))()`).catch(() => null);
        console.log(`  [DIAG][appsample_sync] sameTabId=${asDiag?.curTabId === asBoot?.tabId} swAckSeq=${swAckSeq} latestSeq=${latestSeq} ${JSON.stringify(asDiag)}`);
      }
      record(
        'appsample_live_sync_position',
        asSyncOk,
        asSyncState
          ? `center=(${asSyncState.center.lat.toFixed(5)},${asSyncState.center.lng.toFixed(5)}) exp=(${asVec.expected_leaflet.lat.toFixed(5)},${asVec.expected_leaflet.lng.toFixed(5)}) markerErrPx=${asSyncState.mErrPx.toFixed(2)} zoom=${asSyncState.zoom} animating=${asSyncState.animating}`
          : 'no sync state'
      );

      // Guest mark check on AppSample
      const asPinId = await asTab.page.eval(`(() => {
        const pins = window.__GH_BRIDGE__.queryNearbyUncollectedPins();
        return pins.length > 0 ? pins[0].id : null;
      })()`);
      const fbBefore = recorded.action_feedback.length;
      pushServerEvent({
        type: 'NAV_MARK_PIN',
        pin_id: asPinId,
        action: 'mark',
        target_provider: 'appsample',
        owner_tab_id: asBoot.tabId
      });
      const asFb = await waitUntil(() => {
        const list = recorded.action_feedback.slice(fbBefore);
        return list.find((x) => x && x.pin_id === asPinId);
      }, 10000, 150);
      const asToast = await asTab.page.eval(`(() => {
        const el = document.getElementById('gh-mini-toast');
        return el ? { visible: el.classList.contains('is-visible'), text: el.textContent } : null;
      })()`);
      const asGuestMarkOk = Boolean(
        asPinId && asFb && asFb.status === 'error' &&
        /sign in/i.test(String(asFb.detail || '')) &&
        asToast && asToast.visible && /sign in/i.test(String(asToast.text || ''))
      );
      if (!asGuestMarkOk) {
        const asDiag = await asTab.page.eval(`(() => ({
          curTabId: window.__GH_BRIDGE__ ? window.__GH_BRIDGE__.tabId : null,
          hasWrap: Boolean(document.querySelector('.gh-player-marker-wrap')),
          url: location.href
        }))()`).catch(() => null);
        console.log(`  [DIAG][appsample_mark] sameTabId=${asDiag?.curTabId === asBoot?.tabId} swAckSeq=${swAckSeq} latestSeq=${latestSeq} ${JSON.stringify(asDiag)}`);
      }
      record(
        'appsample_live_guest_mark_auth_error',
        asGuestMarkOk,
        `pin=${asPinId} status=${asFb?.status} detail="${asFb?.detail}" toastVisible=${asToast?.visible}`
      );

      // GATING: 8-heading walk on AppSample
      const asWalk = await runEightHeadingWalk(asTab.page, 'appsample', -500, 1000);
      const asWalkSummary = `maxArrowErr=${asWalk.maxArrowErr.toFixed(1)}deg maxConeErr=${asWalk.maxConeErr.toFixed(1)}deg maxPosErr=${asWalk.maxPosErr.toFixed(2)}px fov=${asWalk.allFov}`;
      if (asWalk.posOk && !asWalk.orientOk) {
        record('appsample_8heading_walk', false, `${asWalkSummary} samples=${JSON.stringify(asWalk.perHeading)}`, 'AppSample live map orientation mismatch vs arrow/cone heading');
      } else {
        record('appsample_8heading_walk', asWalk.posOk && asWalk.orientOk, asWalkSummary);
      }

      // INFORMATIONAL: ~200-frame QA replay on AppSample
      if (qaFrames) {
        qaResults.appsample = await runQaReplay(asTab.page, 'appsample', qaFrames);
      }
    } finally {
      await closeTab(asTab);
      flushEventQueue();
    }

    // =========================================================================
    // PROVIDER 2: Kongying v3 (https://v3.yuanshen.site/)
    // =========================================================================
    console.log('\n[2/3] Kongying v3 live checks');
    const kyTab = await openTabWithDiagnostics(KONGYING_URL);
    const kyDiag = kyTab.diag;
    let kyDumped = false;
    const ensureKyDumped = async (extra = {}) => {
      const d = await dumpKongyingDiagnostics(kyTab.page, kyDiag, extra);
      kyDumped = true;
      return d;
    };
    try {
      const probeKongyingBoot = () => kyTab.page.eval(`(() => {
        ${KONGYING_FIND_ITEM_SELECTOR_JS}
        const b = window.__GH_BRIDGE__;
        if (!b) return null;
        const ctx = b.getContext();
        if (!ctx || !ctx.vm || !ctx.mainStore || !ctx.map) return null;
        const ms = ctx.mainStore;
        const vm = ctx.vm;
        const areaOk = Array.isArray(ms.area_list) && ms.area_list.length > 0 && Boolean(ms.selected_child_area && ms.selected_child_area.id);
        const tpOk = Array.isArray(ms.teleport_list) && ms.teleport_list.length > 0;
        const iconOk = Array.isArray(vm.icon_list) && vm.icon_list.length > 0;
        const mapInitOk = Boolean(vm.BXGroup && vm.teleport_map);
        const isel = findItemSelector(vm);
        const iselVueOk = Boolean(isel && !isel.item_loading && Array.isArray(isel.type_list) && isel.type_list.length > 0);
        const visHeaders = Array.from(document.querySelectorAll('.item_selector_item .item_header'))
          .filter((el) => el.offsetWidth > 0 && el.offsetHeight > 0).length;
        const domSpinner = Boolean(document.querySelector('.item_selector .q-inner-loading'));
        const iselOk = iselVueOk || (!isel && visHeaders >= 2 && !domSpinner);
        if (!areaOk || !tpOk || !iconOk || !mapInitOk || !iselOk) return null;
        return {
          providerId: b.providerId,
          tabId: b.tabId,
          hasRegister: typeof window.__GH_REGISTER_MAP_PROVIDER__ === 'function',
          mapId: b.getCurrentMapId(),
          areaCount: ms.area_list.length,
          teleportListLen: ms.teleport_list.length,
          iconListLen: vm.icon_list.length,
          hasItemSelectorProxy: Boolean(isel)
        };
      })()`);

      let itemListRefreshTried = false;
      const runKongyingBootAttempt = async (maxMs, { allowEarlyOauthAbort = false } = {}) => {
        const tAttempt0 = Date.now();
        let attemptItemRefreshTried = false;
        while (Date.now() - tAttempt0 < maxMs) {
          let res = null;
          try { res = await probeKongyingBoot(); } catch { res = null; }
          if (res) return res;

          const elapsed = Date.now() - tAttempt0;
          if (elapsed >= 18000) {
            if (allowEarlyOauthAbort) {
              const nowOffset = Date.now() - kyDiag.t0;
              const deadOauth = [...kyDiag.requests.values()].some((r) =>
                r.url.includes('/oauth/token') &&
                r.status === null &&
                r.tEnd === null &&
                (nowOffset - r.t0) >= 15000
              );
              if (deadOauth) return null;
            }
            if (!attemptItemRefreshTried) {
              const refreshed = await kyTab.page.eval(`(async () => {
                ${KONGYING_FIND_ITEM_SELECTOR_JS}
                const ctx = window.__GH_BRIDGE__ && window.__GH_BRIDGE__.getContext();
                if (!ctx || !ctx.vm || !ctx.mainStore) return false;
                const ms = ctx.mainStore;
                const isel = findItemSelector(ctx.vm);
                const areaId = ms.selected_child_area && ms.selected_child_area.id;
                const tpLen = Array.isArray(ms.teleport_list) ? ms.teleport_list.length : 0;
                if (Array.isArray(ms.area_list) && ms.area_list.length > 0 && areaId && Array.isArray(ctx.vm.icon_list) && ctx.vm.icon_list.length > 0 && tpLen === 0 && isel && typeof isel.get_itemlist === 'function') {
                  await isel.get_itemlist(areaId);
                  return true;
                }
                return false;
              })()`).catch(() => false);
              if (refreshed) {
                attemptItemRefreshTried = true;
                itemListRefreshTried = true;
              }
            }
          }
          await sleep(300);
        }
        return null;
      };

      let kyBoot = await runKongyingBootAttempt(45000, { allowEarlyOauthAbort: true });

      let bootReloaded = false;
      if (!kyBoot) {
        const stallCheck = classifyKongyingSiteIssue(kyDiag, { pendingThresholdMs: 8000 });
        if (stallCheck.unavailable) {
          bootReloaded = true;
          console.log(`  [INFO] kongying boot stalled (${stallCheck.summary}); reloading once to recover`);
          kyDiag.resetWindow('boot_stall_reload');
          await kyTab.page.send('Page.reload', { ignoreCache: false });
          kyBoot = await runKongyingBootAttempt(55000, { allowEarlyOauthAbort: false });
        }
      }

      const kyBootOk = Boolean(kyBoot && kyBoot.providerId === 'kongying' && kyBoot.hasRegister && kyBoot.mapId === 2);
      if (!kyBootOk) {
        const siteIssue = classifyKongyingSiteIssue(kyDiag, { pendingThresholdMs: 8000 });
        await ensureKyDumped({ phase: 'boot', bootReloaded, itemListRefreshTried, kyBoot });
        const bootReason = siteIssue.unavailable ? `site_unavailable: ${siteIssue.summary}` : JSON.stringify(kyBoot);
        const depReason = siteIssue.unavailable ? `site_unavailable: ${siteIssue.summary}` : 'boot failed (no context)';
        record('kongying_live_boot_and_context', false, bootReason);
        record('kongying_live_calibrate_provider', false, depReason);
        record('kongying_live_sync_position', false, depReason);
        record('kongying_live_guest_mark_unmark', false, depReason);
        record('kongying_8heading_walk', false, depReason);
      } else {
        record('kongying_live_boot_and_context', true, JSON.stringify(kyBoot));

        // Enable teleports only after teleport_list & map state are ready, so teleport_map never caches an empty LayerGroup
        const countTeleportLayersJs = `(() => {
          const ctx = window.__GH_BRIDGE__ && window.__GH_BRIDGE__.getContext();
          if (!ctx || !ctx.map || !ctx.map._layers) return { tpLayers: 0, tpGroup: -1, loading: false };
          let tpLayers = 0;
          const seen = new Set();
          const visit = (l) => {
            if (!l || seen.has(l)) return;
            seen.add(l);
            const d = l.options && l.options.data;
            if (d && typeof d.position === 'string') {
              const title = String(d.markerTitle || '').trim();
              const isTp = Boolean(l.options && l.options.icon && l.options.icon.options && l.options.icon.options.teleport) ||
                /^(传送锚点|七天神像|秘境|征讨领域|浪船锚点|奖励秘境)$/.test(title);
              if (isTp) tpLayers++;
            }
            if (typeof l.getLayers === 'function') {
              try { for (const c of l.getLayers()) visit(c); } catch {}
            } else if (typeof l.eachLayer === 'function') {
              try { l.eachLayer(visit); } catch {}
            }
          };
          for (const l of Object.values(ctx.map._layers)) visit(l);
          return {
            tpLayers,
            tpGroup: ctx.vm && ctx.vm.teleport_group && typeof ctx.vm.teleport_group.getLayers === 'function' ? ctx.vm.teleport_group.getLayers().length : -1,
            loading: Boolean(ctx.vm && ctx.vm.loading)
          };
        })()`;

        await kyTab.page.eval(`(() => {
          const ctx = window.__GH_BRIDGE__.getContext();
          const areaName = ctx.mainStore && ctx.mainStore.selected_child_area ? String(ctx.mainStore.selected_child_area.name || '') : '';
          if (areaName && ctx.vm && ctx.vm.teleport_map && typeof ctx.vm.teleport_map.has === 'function' && ctx.vm.teleport_map.has(areaName)) {
            const cached = ctx.vm.teleport_map.get(areaName);
            if (!cached || typeof cached.getLayers !== 'function' || cached.getLayers().length === 0) {
              ctx.vm.teleport_map.delete(areaName);
            }
          }
          if (ctx.vm && !ctx.vm.teleport_state && typeof ctx.vm.teleport_switch === 'function') {
            ctx.vm.teleport_switch();
          }
        })()`);

        let tpReady = await waitUntil(async () => {
          const st = await kyTab.page.eval(countTeleportLayersJs);
          return st && st.tpLayers >= 3 ? st : null;
        }, 20000, 300);

        let teleportRetryUsed = false;
        if (!tpReady) {
          teleportRetryUsed = true;
          await kyTab.page.eval(`(() => {
            const ctx = window.__GH_BRIDGE__.getContext();
            const areaName = ctx.mainStore && ctx.mainStore.selected_child_area ? String(ctx.mainStore.selected_child_area.name || '') : '';
            if (areaName && ctx.vm && ctx.vm.teleport_map) {
              ctx.vm.teleport_map.delete(areaName);
            }
            if (ctx.vm && typeof ctx.vm.teleport_layer_init === 'function') {
              ctx.vm.teleport_state = true;
              ctx.vm.teleport_layer_init();
            }
          })()`);
          tpReady = await waitUntil(async () => {
            const st = await kyTab.page.eval(countTeleportLayersJs);
            return st && st.tpLayers >= 3 ? st : null;
          }, 15000, 300);
        }

        // Select collectible categories via Vue ItemSelector API (with exact DOM .item_body .item_option fallback)
        const selectKongyingCollectibles = async (attemptIdx, excludeIds) => {
          const vueRes = await kyTab.page.eval(`(async () => {
            ${KONGYING_FIND_ITEM_SELECTOR_JS}
            const ctx = window.__GH_BRIDGE__.getContext();
            const ms = ctx.mainStore;
            const isel = findItemSelector(ctx.vm);
            if (!isel) return { foundProxy: false };
            const countAllItems = () => {
              if (!Array.isArray(isel.item_list)) return 0;
              let n = 0;
              for (const arr of isel.item_list) {
                if (Array.isArray(arr)) n += arr.length;
              }
              return n;
            };
            let refetchedItems = false;
            if (countAllItems() === 0 && typeof isel.get_itemlist === 'function' && ms.selected_child_area && ms.selected_child_area.id) {
              refetchedItems = true;
              await isel.get_itemlist(ms.selected_child_area.id);
            }
            const excludeSet = new Set(${JSON.stringify(excludeIds)});
            const selectedSet = new Set((ms.selected_item_list || []).map((x) => x && x.id).filter((id) => id !== undefined));
            const all = [];
            if (Array.isArray(isel.item_list)) {
              for (const arr of isel.item_list) {
                if (!Array.isArray(arr)) continue;
                for (const it of arr) {
                  if (!it || it.id === undefined || Number(it.count || 0) <= 0) continue;
                  if (excludeSet.has(it.id) || selectedSet.has(it.id)) continue;
                  all.push(it);
                }
              }
            }
            const moderate = all.filter((it) => it.count >= 5 && it.count <= 150).sort((a, b) => a.count - b.count);
            const fallback = [...all].sort((a, b) => a.count - b.count);
            const picked = (moderate.length ? moderate : fallback).slice(0, 2);
            for (let i = 0; i < picked.length; i++) {
              isel.insert_selected_item(picked[i]);
              if (i + 1 < picked.length) {
                await new Promise((r) => setTimeout(r, 60));
              }
            }
            return {
              foundProxy: true,
              method: 'vue_item_selector',
              refetchedItems,
              candidateCount: all.length,
              pickedIds: picked.map((x) => x.id),
              picked: picked.map((x) => ({ id: x.id, name: x.name, count: x.count }))
            };
          })()`);
          if (vueRes && vueRes.foundProxy && vueRes.pickedIds && vueRes.pickedIds.length > 0) {
            return vueRes;
          }

          // DOM fallback if ItemSelector proxy is absent or empty
          const expandInfo = await kyTab.page.eval(`(() => {
            const items = Array.from(document.querySelectorAll('.item_selector_item'))
              .filter((item) => {
                const hdr = item.querySelector('.item_header');
                return hdr && hdr.offsetWidth > 0 && hdr.offsetHeight > 0;
              });
            if (!items.length) return { headerCount: 0 };
            const target = items[${attemptIdx} % items.length];
            const alreadyOpen = target.querySelectorAll('.item_body .item_option').length > 0;
            if (!alreadyOpen) {
              const hdr = target.querySelector('.item_header');
              if (hdr) hdr.click();
            }
            return { headerCount: items.length, targetIndex: ${attemptIdx} % items.length, alreadyOpen };
          })()`);
          const optionsReady = await waitUntil(() => kyTab.page.eval(`(() => {
            const opts = Array.from(document.querySelectorAll('.item_selector_item .item_body .item_option'))
              .filter((el) => el.offsetWidth > 0 && el.offsetHeight > 0);
            return opts.length > 0 ? opts.length : null;
          })()`), 5000, 150);
          const clickedOpts = await kyTab.page.eval(`(async () => {
            const opts = Array.from(document.querySelectorAll('.item_selector_item .item_body .item_option:not(.on)'))
              .filter((el) => el.offsetWidth > 0 && el.offsetHeight > 0);
            const clicked = [];
            for (const el of opts.slice(0, 2)) {
              clicked.push((el.textContent || '').trim().slice(0, 40));
              el.click();
              await new Promise((r) => setTimeout(r, 60));
            }
            return clicked;
          })()`);
          return {
            foundProxy: Boolean(vueRes && vueRes.foundProxy),
            method: 'dom_fallback',
            expandInfo,
            optionsReady,
            clickedOpts,
            pickedIds: []
          };
        };

        const selectionReports = [];
        const sel1 = await selectKongyingCollectibles(0, []);
        selectionReports.push(sel1);
        let kyMarkersLoaded = await waitUntil(async () => {
          const n = await kyTab.page.eval(`window.__GH_BRIDGE__.queryNearbyUncollectedPins().length`);
          return n > 0 ? n : null;
        }, 20000, 300);

        if (!kyMarkersLoaded) {
          const sel2 = await selectKongyingCollectibles(1, sel1.pickedIds || []);
          selectionReports.push(sel2);
          kyMarkersLoaded = await waitUntil(async () => {
            const n = await kyTab.page.eval(`window.__GH_BRIDGE__.queryNearbyUncollectedPins().length`);
            return n > 0 ? n : null;
          }, 20000, 300);
        }

        await kyTab.page.eval(`window.__GH_BRIDGE__ && window.__GH_BRIDGE__.calibrateNow()`);
        const kyCal = await waitUntil(
          () => [...recorded.calibrate_provider].reverse().find((r) => r && r.provider === 'kongying'),
          10000,
          200
        );
        const kyCalOk = Boolean(kyCal && kyCal.provider === 'kongying' && kyCal.map_id === 2 && kyCal.swap_lat_lng === true && Array.isArray(kyCal.anchors) && kyCal.anchors.length >= 3);
        let kyCalDetail = kyCal
          ? `provider=${kyCal.provider} map_id=${kyCal.map_id} swap=${kyCal.swap_lat_lng} anchors=${kyCal.anchors.length} pins=${kyMarkersLoaded}`
          : `missing (pins=${kyMarkersLoaded})`;
        if (!kyCalOk) {
          const diagRes = await ensureKyDumped({
            phase: 'calibrate_provider',
            teleportRetryUsed,
            tpReady,
            selectionReports,
            kyMarkersLoaded
          });
          const leafletTp = diagRes.pageState && diagRes.pageState.leaflet ? diagRes.pageState.leaflet.leafletTeleportLayers : 0;
          const siteIssue = classifyKongyingSiteIssue(kyDiag, { pendingThresholdMs: 8000 });
          if (leafletTp < 3 && siteIssue.unavailable) {
            kyCalDetail = `site_unavailable: ${siteIssue.summary} (${kyCalDetail})`;
          }
        }
        record('kongying_live_calibrate_provider', kyCalOk, kyCalDetail);

        // SYNC_POSITION_ONLY check against transform_vectors.json (kongying_m2_q4: cx=1000, cy=-500)
        const kyVec = vectorsDoc.vectors.find((v) => v.id === 'kongying_m2_q4');
        await pushAndWaitDelivered({
          type: 'SYNC_POSITION_ONLY',
          map_id: 2,
          lat: kyVec.canonical.y,
          lng: kyVec.canonical.x,
          heading: 0,
          camera_heading: 0,
          zoom_ratio_t: 0.6,
          has_world_coords: true,
          is_underground: false
        }, 100);
        let kySyncState = null;
        await waitUntil(async () => {
          const s = await kyTab.page.eval(`(() => {
            const ctx = window.__GH_BRIDGE__.getContext();
            const c = ctx.map.getCenter();
            const ll = window.GHCoordTransform.canonicalToLeaflet(window.__GH_BRIDGE__.getTransform(), ${kyVec.canonical.y}, ${kyVec.canonical.x});
            const pExp = ctx.map.latLngToContainerPoint(ll);
            const wrap = document.querySelector('.gh-player-marker-wrap');
            const cRect = ctx.map.getContainer().getBoundingClientRect();
            const wRect = wrap ? wrap.getBoundingClientRect() : null;
            const mErrPx = wRect ? Math.hypot((wRect.left + wRect.width / 2 - cRect.left) - pExp.x, (wRect.top + wRect.height / 2 - cRect.top) - pExp.y) : 999;
            return {
              center: { lat: c.lat, lng: c.lng },
              expected: { lat: ll[0], lng: ll[1] },
              zoom: ctx.map.getZoom(),
              animating: Boolean(ctx.map._animatingZoom),
              mErrPx
            };
          })()`);
          if (s) kySyncState = s;
          return Boolean(
            s &&
            !s.animating &&
            Math.abs(s.center.lat - kyVec.expected_leaflet.lat) < 50 &&
            Math.abs(s.center.lng - kyVec.expected_leaflet.lng) < 50 &&
            s.mErrPx <= 8
          );
        }, 5000, 150);
        const kySyncOk = Boolean(
          kySyncState &&
          Math.abs(kySyncState.center.lat - kyVec.expected_leaflet.lat) < 50 &&
          Math.abs(kySyncState.center.lng - kyVec.expected_leaflet.lng) < 50 &&
          kySyncState.mErrPx <= 8
        );
        if (!kySyncOk && !kyDumped) {
          await ensureKyDumped({ phase: 'sync_position', kySyncState });
        }
        record(
          'kongying_live_sync_position',
          kySyncOk,
          kySyncState
            ? `center=(${kySyncState.center.lat.toFixed(1)},${kySyncState.center.lng.toFixed(1)}) exp=(${kyVec.expected_leaflet.lat.toFixed(1)},${kyVec.expected_leaflet.lng.toFixed(1)}) markerErrPx=${kySyncState.mErrPx.toFixed(2)} zoom=${kySyncState.zoom} animating=${kySyncState.animating}`
            : 'no sync state'
        );

        // Guest mark + unmark check on Kongying v3
        const kyPinInfo = await kyTab.page.eval(`(() => {
          const pins = window.__GH_BRIDGE__.queryNearbyUncollectedPins();
          if (!pins.length) return null;
          return { id: pins[0].id, rawId: pins[0].rawId };
        })()`);
        let kyMarkOk = false;
        let kyMarkDetail = 'no pins available';
        if (kyPinInfo) {
          const fbStart = recorded.action_feedback.length;
          pushServerEvent({
            type: 'NAV_MARK_PIN',
            pin_id: kyPinInfo.id,
            action: 'mark',
            target_provider: 'kongying',
            owner_tab_id: kyBoot.tabId
          });
          const fbMark = await waitUntil(() => recorded.action_feedback.slice(fbStart).find((x) => x && x.pin_id === kyPinInfo.id && x.action === 'mark'), 10000, 150);
          const afterMarkHas = await kyTab.page.eval(`(() => {
            const arr = JSON.parse(localStorage.getItem('marked_layers') || '[]').map(String);
            return arr.includes(${JSON.stringify(kyPinInfo.rawId)});
          })()`);

          const fbStart2 = recorded.action_feedback.length;
          pushServerEvent({
            type: 'NAV_MARK_PIN',
            pin_id: kyPinInfo.id,
            action: 'unmark',
            target_provider: 'kongying',
            owner_tab_id: kyBoot.tabId
          });
          const fbUnmark = await waitUntil(() => recorded.action_feedback.slice(fbStart2).find((x) => x && x.pin_id === kyPinInfo.id && x.action === 'unmark'), 10000, 150);
          const afterUnmarkHas = await kyTab.page.eval(`(() => {
            const arr = JSON.parse(localStorage.getItem('marked_layers') || '[]').map(String);
            return arr.includes(${JSON.stringify(kyPinInfo.rawId)});
          })()`);

          kyMarkOk = Boolean(afterMarkHas && !afterUnmarkHas && fbMark?.status === 'ok' && fbUnmark?.status === 'ok');
          kyMarkDetail = `pin=${kyPinInfo.id} afterMark=${afterMarkHas} afterUnmark=${afterUnmarkHas} fbMark=${fbMark?.status} fbUnmark=${fbUnmark?.status}`;
        }
        if (!kyMarkOk) {
          const diagRes = kyDumped ? null : await ensureKyDumped({ phase: 'guest_mark_unmark', selectionReports, kyMarkersLoaded });
          if (!kyPinInfo) {
            const leafletPins = diagRes && diagRes.pageState && diagRes.pageState.leaflet ? diagRes.pageState.leaflet.leafletPinLayers : 0;
            const siteIssue = classifyKongyingSiteIssue(kyDiag, { pendingThresholdMs: 8000 });
            if (leafletPins === 0 && siteIssue.unavailable) {
              kyMarkDetail = `site_unavailable: ${siteIssue.summary} (no pins available)`;
            }
          }
        }
        record('kongying_live_guest_mark_unmark', kyMarkOk, kyMarkDetail);

        // GATING: 8-heading walk on Kongying v3
        const kyWalk = await runEightHeadingWalk(kyTab.page, 'kongying', -500, 1000);
        const kyWalkSummary = `maxArrowErr=${kyWalk.maxArrowErr.toFixed(1)}deg maxConeErr=${kyWalk.maxConeErr.toFixed(1)}deg maxPosErr=${kyWalk.maxPosErr.toFixed(2)}px fov=${kyWalk.allFov}`;
        if (kyWalk.posOk && !kyWalk.orientOk) {
          record(
            'kongying_8heading_walk',
            false,
            `${kyWalkSummary} samples=${JSON.stringify(kyWalk.perHeading)}`,
            'Kongying v3 live map screen motion orientation differs from arrow/cone CSS heading while position is accurate'
          );
        } else {
          if (!(kyWalk.posOk && kyWalk.orientOk) && !kyDumped) {
            await ensureKyDumped({ phase: '8heading_walk', kyWalkSummary });
          }
          record('kongying_8heading_walk', kyWalk.posOk && kyWalk.orientOk, (kyWalk.posOk && kyWalk.orientOk) ? kyWalkSummary : `${kyWalkSummary} samples=${JSON.stringify(kyWalk.perHeading)}`);
        }

        // INFORMATIONAL: ~200-frame QA replay on Kongying v3
        if (qaFrames) {
          qaResults.kongying = await runQaReplay(kyTab.page, 'kongying', qaFrames);
        }
      }
    } finally {
      await closeTab(kyTab);
      flushEventQueue();
    }

    // =========================================================================
    // PROVIDER 3: HoYoLAB (https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2)
    // =========================================================================
    console.log('\n[3/3] HoYoLAB live orientation checks (guest)');
    const hlTab = await openTab(HOYOLAB_URL);
    try {
      const hlBoot = await waitUntil(async () => {
        return hlTab.page.eval(`(() => {
          if (!window.__gh_booted_build) return null;
          const map = ${providerMapAccessorJs('hoyolab')};
          if (!map || typeof map.latLngToContainerPoint !== 'function') return null;
          return {
            build: window.__gh_booted_build,
            zoom: map.getZoom(),
            hasProviderRegister: typeof window.__GH_REGISTER_MAP_PROVIDER__,
            hasProviderStyles: Boolean(document.getElementById('gh-provider-styles'))
          };
        })()`);
      }, 60000, 500);
      record(
        'hoyolab_live_boot_and_isolation',
        Boolean(hlBoot && hlBoot.build && hlBoot.hasProviderRegister === 'undefined' && !hlBoot.hasProviderStyles),
        JSON.stringify(hlBoot)
      );

      // GATING: 8-heading walk on HoYoLAB
      const hlWalk = await runEightHeadingWalk(hlTab.page, 'hoyolab', -500, 1000);
      const hlWalkSummary = `maxArrowErr=${hlWalk.maxArrowErr.toFixed(1)}deg maxConeErr=${hlWalk.maxConeErr.toFixed(1)}deg maxPosErr=${hlWalk.maxPosErr.toFixed(2)}px fov=${hlWalk.allFov}`;
      if (hlWalk.posOk && !hlWalk.orientOk) {
        record(
          'hoyolab_8heading_walk',
          false,
          `${hlWalkSummary} samples=${JSON.stringify(hlWalk.perHeading)}`,
          'HoYoLAB live map screen motion orientation differs from arrow/cone CSS heading while position is accurate'
        );
      } else {
        record('hoyolab_8heading_walk', hlWalk.posOk && hlWalk.orientOk, hlWalkSummary);
      }

      // INFORMATIONAL: ~200-frame QA replay on HoYoLAB
      if (qaFrames) {
        qaResults.hoyolab = await runQaReplay(hlTab.page, 'hoyolab', qaFrames);
      }
    } finally {
      await closeTab(hlTab);
    }

    // =========================================================================
    // INFORMATIONAL: QA Replay Summary (never fails the run)
    // =========================================================================
    console.log('\n[INFO] 200-frame QA replay summary (sess_20261127_211043):');
    if (!qaFrames) {
      console.log('  [INFO] scratch/qa_sessions/sess_20261127_211043/meta.jsonl not found; skipping informational replay.');
    } else {
      for (const p of ['appsample', 'kongying', 'hoyolab']) {
        const r = qaResults[p];
        if (!r) continue;
        console.log(`  [INFO] ${p}: frames=${r.framesFed} moving_steps=${r.movingSteps} arrow_vs_screen_within_30deg=${r.agreeSteps}/${r.movingSteps} (${r.agreePct}%)`);
      }
      // Cross-provider agreement across all 3 providers on common step indices
      const byIdx = new Map();
      for (const p of ['appsample', 'kongying', 'hoyolab']) {
        const r = qaResults[p];
        if (!r) continue;
        for (const s of r.stepRecords) {
          if (!byIdx.has(s.idx)) byIdx.set(s.idx, {});
          byIdx.get(s.idx)[p] = s;
        }
      }
      let common = 0;
      let screenAllAgree = 0;
      let arrowAllAgree = 0;
      let coneAllAgree = 0;
      for (const entry of byIdx.values()) {
        if (!entry.appsample || !entry.kongying || !entry.hoyolab) continue;
        common++;
        const sOk = angularDiff(entry.appsample.screenAngle, entry.hoyolab.screenAngle) <= 30 &&
          angularDiff(entry.kongying.screenAngle, entry.hoyolab.screenAngle) <= 30;
        const aOk = angularDiff(entry.appsample.arrowDeg, entry.hoyolab.arrowDeg) <= 10 &&
          angularDiff(entry.kongying.arrowDeg, entry.hoyolab.arrowDeg) <= 10;
        const cOk = angularDiff(entry.appsample.coneDeg, entry.hoyolab.coneDeg) <= 10 &&
          angularDiff(entry.kongying.coneDeg, entry.hoyolab.coneDeg) <= 10;
        if (sOk) screenAllAgree++;
        if (aOk) arrowAllAgree++;
        if (cOk) coneAllAgree++;
      }
      const pct = (n) => (common > 0 ? ((n / common) * 100).toFixed(1) : '0.0');
      console.log(`  [INFO] cross-provider (n=${common}): screen_angle_agree=${screenAllAgree}/${common} (${pct(screenAllAgree)}%) arrow_css_agree=${arrowAllAgree}/${common} (${pct(arrowAllAgree)}%) cone_css_agree=${coneAllAgree}/${common} (${pct(coneAllAgree)}%)`);
    }
    console.log(`\n[INFO] extension service worker started ${swStarts} time(s) (ready=${swReadyCount}, crashed=${swCrashedCount}); app mock attached each time`);
  } finally {
    await killAndClean();
  }

  const unexpectedFails = results.filter((r) => r.status === 'FAIL');
  const knownFails = results.filter((r) => r.status === 'KNOWN-FAIL');
  const passed = results.filter((r) => r.status === 'OK');

  if (unexpectedFails.length > 0) {
    console.log(`\nPROVIDER LIVE: FAILED ${unexpectedFails.length}/${results.length} (pass=${passed.length}, known-fail=${knownFails.length})`);
    return 1;
  }
  if (knownFails.length > 0) {
    console.log(`\nPROVIDER LIVE: ALL ${passed.length} CHECKS PASSED (${knownFails.length} known-fail)`);
    return 0;
  }
  console.log(`\nPROVIDER LIVE: ALL ${results.length} CHECKS PASSED`);
  return 0;
}

const watchdog = setTimeout(() => {
  console.error(`[ERROR] provider live smoke exceeded the ${WATCHDOG_MS} ms watchdog; aborting`);
  if (abortRun) abortRun();
  process.exit(1);
}, WATCHDOG_MS);
void watchdog;

main().then((code) => process.exit(code), (err) => {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  process.exit(1);
});
