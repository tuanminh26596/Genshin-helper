// Real-browser DOM fixture tests for HoYoLAB pin-popup mark-button detection (Node >= 22, zero npm deps).
//
// Extracts the exact production popup-helper functions directly from extension/page_main.js at
// runtime (never copied/hardcoded), loads captured real HoYoLAB pin-popup HTML fixtures in headless
// Chromium via CDP, and asserts that findGhimButton / findAlreadyMarkedButton select the primary
// Mark button (distinguished from Batch Mark, disabled, and already-marked buttons).
//
// Usage:
//   node tests/extension_dom_fixtures.mjs
// Optional env:
//   GH_CHROME=<chrome.exe>          Chromium executable override (else Playwright Chromium)
//   GH_PAGE_MAIN_PATH=<path.js>     Override source JS file (for seeded xpass proofs only)
// Exit code: 0 iff fail === 0, 1 on fail/error, 2 if Chromium not found.
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const FIXTURES_DIR = join(ROOT, 'tests', 'fixtures', 'hoyolab_popup');
const MANIFEST_PATH = join(FIXTURES_DIR, 'manifest.json');
const PAGE_MAIN_PATH = process.env.GH_PAGE_MAIN_PATH
  ? resolve(process.env.GH_PAGE_MAIN_PATH)
  : join(ROOT, 'extension', 'page_main.js');

const REQUIRED_FUNCTIONS = [
  'getText',
  '_isVisibleEl',
  'findPopupContainer',
  'isButtonDisabled',
  'normalizeBtnText',
  'findGhimButton',
  'findAlreadyMarkedButton',
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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

/**
 * Extract the exact source of a top-level or nested `function <name>(...) { ... }` declaration
 * from JS source using a lexer-aware balanced-brace scanner. Fails loudly if missing/unterminated.
 */
function extractFunctionSource(src, name) {
  const re = new RegExp(`\\bfunction\\s+${name}\\s*\\(`);
  const match = re.exec(src);
  if (!match) {
    throw new Error(`Required function "${name}" not found in ${PAGE_MAIN_PATH}`);
  }
  const startIdx = match.index;
  // Find parameter-list closing ')' and opening '{'
  let i = startIdx + match[0].length;
  let parenDepth = 1;
  while (i < src.length && parenDepth > 0) {
    const ch = src[i];
    if (ch === "'" || ch === '"' || ch === '`') {
      i = skipString(src, i);
      continue;
    }
    if (ch === '/' && src[i + 1] === '/') {
      i = skipLineComment(src, i);
      continue;
    }
    if (ch === '/' && src[i + 1] === '*') {
      i = skipBlockComment(src, i);
      continue;
    }
    if (ch === '(') parenDepth++;
    else if (ch === ')') parenDepth--;
    i++;
  }
  if (parenDepth !== 0) {
    throw new Error(`Unterminated parameter list for function "${name}" in ${PAGE_MAIN_PATH}`);
  }
  while (i < src.length && /\s/.test(src[i])) i++;
  if (src[i] !== '{') {
    throw new Error(`Expected "{" after parameters of function "${name}" in ${PAGE_MAIN_PATH}`);
  }

  let braceDepth = 0;
  let prevNonWs = '{';
  while (i < src.length) {
    const ch = src[i];
    if (ch === "'" || ch === '"' || ch === '`') {
      i = skipString(src, i);
      prevNonWs = ch;
      continue;
    }
    if (ch === '/' && src[i + 1] === '/') {
      i = skipLineComment(src, i);
      continue;
    }
    if (ch === '/' && src[i + 1] === '*') {
      i = skipBlockComment(src, i);
      continue;
    }
    if (ch === '/' && canStartRegex(prevNonWs)) {
      i = skipRegexLiteral(src, i);
      prevNonWs = '/';
      continue;
    }
    if (ch === '{') {
      braceDepth++;
      prevNonWs = '{';
    } else if (ch === '}') {
      braceDepth--;
      prevNonWs = '}';
      if (braceDepth === 0) {
        return src.slice(startIdx, i + 1);
      }
    } else if (!/\s/.test(ch)) {
      prevNonWs = ch;
    }
    i++;
  }
  throw new Error(`Unterminated body for function "${name}" in ${PAGE_MAIN_PATH}`);
}

function skipString(src, start) {
  const quote = src[start];
  let i = start + 1;
  while (i < src.length) {
    const ch = src[i];
    if (ch === '\\') {
      i += 2;
      continue;
    }
    if (quote === '`' && ch === '$' && src[i + 1] === '{') {
      // Skip template expression conservatively by balancing braces inside ${...}
      i += 2;
      let depth = 1;
      while (i < src.length && depth > 0) {
        if (src[i] === "'" || src[i] === '"' || src[i] === '`') {
          i = skipString(src, i);
          continue;
        }
        if (src[i] === '{') depth++;
        else if (src[i] === '}') depth--;
        i++;
      }
      continue;
    }
    if (ch === quote) return i + 1;
    i++;
  }
  throw new Error(`Unterminated string literal starting at index ${start}`);
}

function skipLineComment(src, start) {
  let i = start + 2;
  while (i < src.length && src[i] !== '\n') i++;
  return i;
}

function skipBlockComment(src, start) {
  const end = src.indexOf('*/', start + 2);
  if (end === -1) throw new Error(`Unterminated block comment starting at index ${start}`);
  return end + 2;
}

function canStartRegex(prevChar) {
  return '=(,:;!?&|[{+-*%^~<>'.includes(prevChar);
}

function skipRegexLiteral(src, start) {
  let i = start + 1;
  let inClass = false;
  while (i < src.length) {
    const ch = src[i];
    if (ch === '\\') {
      i += 2;
      continue;
    }
    if (ch === '[') inClass = true;
    else if (ch === ']') inClass = false;
    else if (ch === '/' && !inClass) {
      i++;
      while (i < src.length && /[a-z]/i.test(src[i])) i++;
      return i;
    } else if (ch === '\n') {
      return start + 1;
    }
    i++;
  }
  return i;
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
        this.pending.get(m.id)(m);
        this.pending.delete(m.id);
      } else if (m.method) {
        for (const h of this.handlers) h(m);
      }
    };
    this.ready = new Promise((r) => { this.ws.onopen = r; });
  }
  send(method, params = {}) {
    return new Promise((resolveSend) => {
      const id = ++this.id;
      this.pending.set(id, resolveSend);
      this.ws.send(JSON.stringify({ id, method, params }));
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
  waitForEvent(method, timeoutMs = 15000) {
    return new Promise((resolveWait, rejectWait) => {
      const timer = setTimeout(() => {
        this.handlers = this.handlers.filter((h) => h !== listener);
        rejectWait(new Error(`Timeout waiting for CDP event ${method}`));
      }, timeoutMs);
      const listener = (m) => {
        if (m.method === method) {
          clearTimeout(timer);
          this.handlers = this.handlers.filter((h) => h !== listener);
          resolveWait(m);
        }
      };
      this.handlers.push(listener);
    });
  }
  close() { try { this.ws.close(); } catch (e) {} }
}

async function main() {
  const chrome = findChrome();
  if (!chrome) {
    console.log('[SKIP] no Chromium found (set GH_CHROME or install Playwright Chromium)');
    return 2;
  }

  if (!existsSync(PAGE_MAIN_PATH)) {
    throw new Error(`page_main.js not found at ${PAGE_MAIN_PATH}`);
  }
  const pageMainSrc = readFileSync(PAGE_MAIN_PATH, 'utf8');
  const extractedBundle = REQUIRED_FUNCTIONS.map((fn) => extractFunctionSource(pageMainSrc, fn)).join('\n\n');

  if (!existsSync(MANIFEST_PATH)) {
    throw new Error(`Fixture manifest not found at ${MANIFEST_PATH}`);
  }
  const manifest = JSON.parse(readFileSync(MANIFEST_PATH, 'utf8'));
  const fixtures = Array.isArray(manifest.fixtures) ? manifest.fixtures : [];
  if (fixtures.length === 0) {
    throw new Error(`No fixtures listed in ${MANIFEST_PATH}`);
  }

  const profile = mkdtempSync(join(tmpdir(), 'gh_dom_fixtures_'));
  const proc = spawn(chrome, [
    '--headless=new',
    '--remote-debugging-port=0',
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--window-size=1440,900',
    'about:blank',
  ], { stdio: 'ignore' });

  const sessions = [];
  const counts = { pass: 0, fail: 0, 'known-fail': 0, xpass: 0 };

  try {
    let port = null;
    for (let i = 0; i < 150 && !port; i++) {
      await sleep(100);
      const f = join(profile, 'DevToolsActivePort');
      if (existsSync(f)) port = readFileSync(f, 'utf8').split('\n')[0].trim();
    }
    if (!port) throw new Error('Chromium did not expose a DevTools port');
    const http = (p, method = 'GET') => fetch(`http://127.0.0.1:${port}${p}`, { method }).then((r) => r.json());

    const pageTarget = await http('/json/new?about:blank', 'PUT');
    const page = new Cdp(pageTarget.webSocketDebuggerUrl);
    sessions.push(page);
    await page.ready;
    await page.send('Runtime.enable');
    await page.send('Page.enable');

    for (const fx of fixtures) {
      const fxPath = join(FIXTURES_DIR, fx.file);
      if (!existsSync(fxPath)) {
        throw new Error(`Fixture HTML not found: ${fxPath}`);
      }
      const loadPromise = page.waitForEvent('Page.loadEventFired', 15000);
      await page.send('Page.navigate', { url: pathToFileURL(fxPath).href });
      await loadPromise;

      const evalRes = await page.eval(`(() => {
        ${extractedBundle}

        const spec = ${JSON.stringify(fx)};
        const popup = findPopupContainer();
        const ghimBtn = findGhimButton();
        const markedBtn = findAlreadyMarkedButton();

        const expectedMarkEl = spec.expected_mark_selector
          ? document.querySelector(spec.expected_mark_selector)
          : null;
        const batchEl = spec.expected_batch_selector
          ? document.querySelector(spec.expected_batch_selector)
          : null;

        const errors = [];
        if (!popup) {
          errors.push('findPopupContainer() returned null');
        }
        if (spec.expected_batch_selector && (!batchEl || batchEl.offsetWidth === 0 || batchEl.offsetHeight === 0)) {
          errors.push('fixture missing visible batch-mark button: ' + spec.expected_batch_selector);
        }
        if (spec.state === 'unmarked') {
          if (!expectedMarkEl || expectedMarkEl.offsetWidth === 0 || expectedMarkEl.offsetHeight === 0) {
            errors.push('fixture missing visible expected mark button: ' + spec.expected_mark_selector);
          }
          if (!ghimBtn) {
            errors.push('findGhimButton() returned null/undefined (expected "' + spec.expected_mark_text + '")');
          } else {
            if (batchEl && ghimBtn === batchEl) {
              errors.push('findGhimButton() matched batch-mark button instead of primary mark button');
            }
            if (expectedMarkEl && ghimBtn !== expectedMarkEl) {
              errors.push('findGhimButton() matched unexpected element: ' + ghimBtn.className + ' ("' + normalizeBtnText(ghimBtn) + '")');
            }
            const actualText = normalizeBtnText(ghimBtn);
            if (spec.expected_mark_text && actualText !== spec.expected_mark_text) {
              errors.push('findGhimButton() text mismatch: got "' + actualText + '", expected "' + spec.expected_mark_text + '"');
            }
            if (isButtonDisabled(ghimBtn)) {
              errors.push('findGhimButton() returned a disabled button');
            }
          }
          if (markedBtn) {
            errors.push('findAlreadyMarkedButton() unexpectedly matched on unmarked fixture: "' + normalizeBtnText(markedBtn) + '"');
          }
        } else if (spec.state === 'marked') {
          if (ghimBtn) {
            errors.push('findGhimButton() unexpectedly matched on marked fixture: "' + normalizeBtnText(ghimBtn) + '"');
          }
          if (!markedBtn) {
            errors.push('findAlreadyMarkedButton() returned null/undefined on marked fixture');
          }
        }

        // Verify disabled primary button is rejected by findGhimButton() if currently matched
        let disabledRejected = true;
        if (ghimBtn && spec.state === 'unmarked') {
          ghimBtn.disabled = true;
          const afterDisable = findGhimButton();
          ghimBtn.disabled = false;
          if (afterDisable) {
            disabledRejected = false;
            errors.push('findGhimButton() did not ignore disabled primary button (returned "' + normalizeBtnText(afterDisable) + '")');
          }
        }

        return {
          ok: errors.length === 0,
          errors,
          popupClass: popup ? popup.className : null,
          ghimText: ghimBtn ? normalizeBtnText(ghimBtn) : null,
          ghimClass: ghimBtn ? ghimBtn.className : null,
          batchText: batchEl ? normalizeBtnText(batchEl) : null,
          markedText: markedBtn ? normalizeBtnText(markedBtn) : null,
          disabledRejected,
        };
      })()`);

      const hasKnownBug = Boolean(fx.known_bug);
      let status;
      if (evalRes.ok && !hasKnownBug) status = 'pass';
      else if (!evalRes.ok && !hasKnownBug) status = 'fail';
      else if (!evalRes.ok && hasKnownBug) status = 'known-fail';
      else status = 'xpass';

      counts[status]++;
      const detail = evalRes.ok
        ? `matched="${evalRes.ghimText}" (batch="${evalRes.batchText}", popup="${evalRes.popupClass}")${hasKnownBug ? ` [known_bug resolved: ${fx.known_bug}]` : ''}`
        : `${evalRes.errors.join('; ')}${hasKnownBug ? ` [known_bug: ${fx.known_bug}]` : ''}`;
      console.log(`  [${status}] ${fx.id} (${fx.lang}, ${fx.state}): ${detail}`);
    }
  } finally {
    for (const c of sessions) c.close();
    proc.kill();
    await sleep(600);
    try { rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 300 }); } catch (e) {}
  }

  console.log(`DOM fixtures: pass ${counts.pass} | fail ${counts.fail} | known-fail ${counts['known-fail']} | xpass ${counts.xpass}`);
  return counts.fail === 0 ? 0 : 1;
}

main().then((code) => process.exit(code), (err) => {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  process.exit(1);
});
