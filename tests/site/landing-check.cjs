// Playwright checks for the landing page in site/ (no build step, so nothing to compile).
//
//   node tests/site/landing-check.cjs [site-dir] [page-path] [--snapshot out.json]
//
// site-dir defaults to ./site, page-path to / (use /ru/ for the Russian page). Serves the
// folder on a local port and checks, at 320, 768, 1280 and 1920 px: no horizontal overflow,
// no console errors, no failed requests and no third-party request other than the two the
// page already makes (Umami's script, GitHub's API for the star count), which are stubbed.
// Then: the install steps, Copy → Copied, the star count, the phone demo, a dictionary with
// keys missing or malformed (falls back to English) and the page with JavaScript off.
// --snapshot writes what the page showed (steps, demo frames, labels) as JSON, to diff two
// versions of the page. Exits non-zero on any failed check.
//
// Needs Node and the `playwright` package with Chromium (not a dependency of the repo:
// `npm i -g playwright && npx playwright install chromium` where it isn't installed).
'use strict';
const http = require('http');
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

const args = process.argv.slice(2);
const snapIdx = args.indexOf('--snapshot');
const snapOut = snapIdx >= 0 ? args.splice(snapIdx, 2)[1] : null;
const ROOT = path.resolve(args[0] || 'site');
const PAGE = args[1] || '/';
const WIDTHS = [320, 768, 1280, 1920];
const TYPES = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.txt': 'text/plain; charset=utf-8', '.jpg': 'image/jpeg', '.xml': 'application/xml' };

const failures = [];
function check(ok, what) {
  console.log((ok ? 'ok   ' : 'FAIL ') + what);
  if (!ok) failures.push(what);
}

function serve() {
  const server = http.createServer((req, res) => {
    let p = decodeURIComponent(new URL(req.url, 'http://x').pathname);
    if (p.endsWith('/')) p += 'index.html';
    const file = path.join(ROOT, p);
    if (!file.startsWith(ROOT) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.writeHead(404); return res.end();
    }
    res.writeHead(200, { 'content-type': TYPES[path.extname(file)] || 'application/octet-stream' });
    fs.createReadStream(file).pipe(res);
  });
  return new Promise((r) => server.listen(0, '127.0.0.1', () => r(server)));
}

// Stubs the page's two known third-party requests; any other one is a failure.
async function guard(page, origin, log) {
  page.on('console', (m) => { if (m.type() === 'error') log.errors.push(m.text()); });
  page.on('pageerror', (e) => log.errors.push(String(e)));
  page.on('requestfailed', (r) => log.failed.push(r.url() + ' ' + (r.failure() || {}).errorText));
  page.on('response', (r) => { if (r.status() >= 400) log.failed.push(r.url() + ' ' + r.status()); });
  await page.route('**/*', (route) => {
    const url = route.request().url();
    if (url.startsWith(origin)) return route.continue();
    if (url === 'https://cloud.umami.is/script.js') return route.fulfill({ status: 200, contentType: 'text/javascript', body: '' });
    if (url === 'https://api.github.com/repos/qelphybox/tg-mcp') {
      return route.fulfill({ status: 200, contentType: 'application/json', headers: { 'access-control-allow-origin': '*' }, body: JSON.stringify({ stargazers_count: 1234 }) });
    }
    log.thirdParty.push(url);
    return route.abort();
  });
}

async function widths(browser, url, origin) {
  for (const w of WIDTHS) {
    const ctx = await browser.newContext({ viewport: { width: w, height: 900 } });
    const page = await ctx.newPage();
    const log = { errors: [], failed: [], thirdParty: [] };
    await guard(page, origin, log);
    await page.goto(url, { waitUntil: 'networkidle' });
    await page.click('[data-app="1"]');
    await page.waitForTimeout(300);
    const over = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    check(over <= 0, `${w}px: no horizontal overflow (scrollWidth - innerWidth = ${over})`);
    check(log.errors.length === 0, `${w}px: no console errors ${log.errors.length ? JSON.stringify(log.errors) : ''}`);
    check(log.failed.length === 0, `${w}px: no failed requests ${log.failed.length ? JSON.stringify(log.failed) : ''}`);
    check(log.thirdParty.length === 0, `${w}px: no new third-party requests ${log.thirdParty.length ? JSON.stringify(log.thirdParty) : ''}`);
    await ctx.close();
  }
}

// What the page shows: install steps per app, copy labels, stars, and every distinct frame
// of one full pass of the phone demo (driven by a fake clock).
async function behaviour(browser, url, origin) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin });
  const page = await ctx.newPage();
  const log = { errors: [], failed: [], thirdParty: [] };
  await guard(page, origin, log);
  await page.clock.install();
  await page.goto(url, { waitUntil: 'networkidle' });
  const snap = { apps: [], copy: [], stars: null, frames: [] };

  const buttons = await page.locator('[data-app]').count();
  for (let i = 0; i < buttons; i++) {
    await page.click(`[data-app="${i}"]`);
    snap.apps.push(await page.evaluate(() => {
      const a = document.querySelector('[data-app-url]');
      return {
        open: !document.querySelector('[data-if="appOpen"]').hidden,
        expanded: Array.from(document.querySelectorAll('[data-app]')).map((b) => b.getAttribute('aria-expanded')),
        steps: Array.from(document.querySelectorAll('#app-steps li')).map((li) => li.innerText.replace(/\s+/g, ' ').trim()),
        href: a.href, event: a.getAttribute('data-umami-event-app'), link: a.innerText.trim(),
        note: document.querySelector('[data-text="appNote"]').textContent
      };
    }));
  }
  await page.click(`[data-app="${buttons - 1}"]`);
  snap.closed = await page.evaluate(() => document.querySelector('[data-if="appOpen"]').hidden);
  await page.click('[data-app="0"]');

  const label = () => page.evaluate(() => document.querySelector('[data-copy]').innerText.trim());
  snap.copy.push(await label());
  await page.click('[data-copy]');
  await page.evaluate(() => new Promise((r) => setTimeout(r, 0)));
  snap.copy.push(await label());
  snap.clipboard = await page.evaluate(() => navigator.clipboard.readText());
  await page.clock.runFor(1700);
  snap.copy.push(await label());

  snap.lang = await page.evaluate(() => document.documentElement.lang);
  snap.stars = await page.evaluate(() => ({
    shown: !document.querySelector('[data-stars]').hidden,
    count: document.querySelector('[data-stars-count]').textContent,
    label: document.querySelector('[data-repo-link]').getAttribute('aria-label')
  }));

  // One full cycle of the three scenarios is under a minute.
  let last = '';
  for (let t = 0; t < 70000; t += 50) {
    const f = await page.evaluate(() => {
      const ph = document.getElementById('phone');
      return ph.getAttribute('aria-label') + '\n' + ph.innerText + '\n' + (document.querySelector('[data-if^="cap"]:not([hidden])') || {}).innerText;
    });
    if (f !== last) { snap.frames.push(f); last = f; }
    await page.clock.runFor(50);
  }
  snap.errors = log.errors.concat(log.failed, log.thirdParty);
  await ctx.close();
  return snap;
}

// A dictionary for a made-up language with keys missing or of the wrong shape: the page
// still works and shows English for those keys.
async function fallback(browser, url, origin) {
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const log = { errors: [], failed: [], thirdParty: [] };
  await guard(page, origin, log);
  const dict = {
    xx: {
      copy: 'KOPY',
      apps: { chatgpt: { note: 'XX note', steps: ['xx 1', 'xx 2', 'xx 3', 'xx 4'] }, claude: 'not an object',
        gemini: { steps: [1, 2, 3] } },
      cases: { sell: { msg: 'XX sell', chips: ['too', 'short'] } }
    }
  };
  await page.route(url, async (route) => {
    const res = await route.fetch();
    let body = await res.text();
    body = body.replace(/<html lang="[^"]*"/, '<html lang="xx"')
      .replace('<script src="/landing.js"></script>', `<script>window.TGMCP_STRINGS = ${JSON.stringify(dict)};</script><script src="/landing.js"></script>`);
    route.fulfill({ response: res, body });
  });
  await page.clock.install();
  await page.goto(url, { waitUntil: 'networkidle' });
  await page.click('[data-app="0"]');
  const r1 = await page.evaluate(() => ({
    copy: document.querySelector('[data-copy]').innerText.trim(),
    note: document.querySelector('[data-text="appNote"]').textContent,
    steps: Array.from(document.querySelectorAll('#app-steps li')).map((li) => li.innerText.replace(/\s+/g, ' ').trim()),
    stars: document.querySelector('[data-stars-count]').textContent,
    starsLabel: document.querySelector('[data-repo-link]').getAttribute('aria-label')
  }));
  check(r1.copy === 'KOPY', `fallback: a key the dictionary has is used (copy = ${r1.copy})`);
  check(r1.note === 'XX note', `fallback: a nested key the dictionary has is used (note = ${r1.note})`);
  check(r1.steps.join('|') === '1 xx 1|2 xx 2|3 xx 3|4 xx 4', `fallback: a language may have another number of steps (${r1.steps.join(' | ')})`);
  check(r1.stars === '1.2k' && r1.starsLabel === 'GitHub, 1234 stars', `fallback: star count and label in English (${r1.stars}, "${r1.starsLabel}")`);
  await page.click('[data-app="1"]');
  const r2 = await page.evaluate(() => document.querySelector('#app-steps').innerText);
  check(/Add custom connector/.test(r2), 'fallback: an app given with the wrong shape falls back to English');
  await page.click('[data-app="2"]');
  const r2b = await page.evaluate(() => document.querySelector('#app-steps').innerText);
  check(/Connected apps/.test(r2b), 'fallback: steps that are not text fall back to English');
  await page.clock.runFor(4000);
  const r3 = await page.evaluate(() => document.getElementById('phone').innerText);
  check(/XX sell/.test(r3) && /Weekend garage sale/.test(r3) && /On it\. Setting up your channel\./.test(r3),
    'fallback: the demo uses the given message, English for the missing intro and the short chip list');
  check(log.errors.length === 0, `fallback: no console errors ${log.errors.length ? JSON.stringify(log.errors) : ''}`);
  await page.close();
}

// What search engines and agents read: JSON-LD that parses with every FAQ entry word
// for word on the page, a canonical URL of its own and reciprocal hreflang links.
async function seo(browser, url) {
  const ctx = await browser.newContext({ javaScriptEnabled: false });
  const page = await ctx.newPage();
  await page.route('**/*', (r) => (r.request().url().startsWith(new URL(url).origin) ? r.continue() : r.abort()));
  await page.goto(url);
  const r = await page.evaluate(() => {
    const norm = (t) => t.replace(/\s+/g, ' ').trim();
    const text = norm(document.body.textContent);
    let parsed = true;
    const faq = [];
    for (const el of document.querySelectorAll('script[type="application/ld+json"]')) {
      try {
        const data = JSON.parse(el.textContent);
        for (const node of data['@graph'] || [data]) {
          if (node['@type'] === 'FAQPage') {
            for (const q of node.mainEntity) faq.push([q.name, q.acceptedAnswer.text]);
          }
        }
      } catch (e) { parsed = false; }
    }
    const missing = faq.filter(([q, a]) => !text.includes(norm(q)) || !text.includes(norm(a))).map(([q]) => q);
    const alt = {};
    for (const l of document.querySelectorAll('link[rel="alternate"][hreflang]')) alt[l.hreflang] = new URL(l.href).pathname;
    const canonical = document.querySelector('link[rel="canonical"]');
    return { parsed, faq: faq.length, missing, alt, canonical: canonical && new URL(canonical.href).pathname, lang: document.documentElement.lang };
  });
  const path = new URL(url).pathname;
  check(r.parsed && r.faq >= 5, `seo: JSON-LD parses, ${r.faq} FAQ entries`);
  check(r.missing.length === 0, `seo: every FAQ entry is on the page word for word ${r.missing.length ? JSON.stringify(r.missing) : ''}`);
  check(r.canonical === path, `seo: canonical is the page itself (${r.canonical})`);
  check(r.alt.en === '/' && r.alt.ru === '/ru/' && r.alt['x-default'] === '/' && r.alt[r.lang] === path,
    `seo: hreflang en, ru, x-default, self included (${JSON.stringify(r.alt)})`);
  await ctx.close();
}

async function noScript(browser, url) {
  const ctx = await browser.newContext({ javaScriptEnabled: false });
  const page = await ctx.newPage();
  await page.route('**/*', (r) => (r.request().url().startsWith(url.replace(/\/[^/]*$/, '')) ? r.continue() : r.abort()));
  await page.goto(url);
  const r = await page.evaluate(() => ({
    h1: (document.querySelector('h1') || {}).textContent || '',
    def: Array.from(document.querySelectorAll("#top p")).some((p) => (document.documentElement.lang === 'en' ? /MCP server/ : /MCP/).test(p.textContent) && p.textContent.length > 120),
    faq: document.querySelectorAll('#faq details').length,
    faqText: (document.querySelector('#faq details p') || {}).textContent || ''
  }));
  check(r.h1.length > 10, `no JS: headline in the page ("${r.h1}")`);
  check(r.def, 'no JS: definition paragraph in the page');
  check(r.faq >= 5 && r.faqText.length > 50, `no JS: FAQ in the page (${r.faq} questions)`);
  await ctx.close();
}

(async () => {
  const server = await serve();
  const origin = `http://127.0.0.1:${server.address().port}`;
  const url = origin + PAGE;
  const browser = await chromium.launch();
  try {
    await widths(browser, url, origin);
    const snap = await behaviour(browser, url, origin);
    check(snap.errors.length === 0, `behaviour: no errors ${snap.errors.length ? JSON.stringify(snap.errors) : ''}`);
    check(snap.apps.length === 3 && snap.apps.every((a) => a.open && a.steps.length === 3), 'each "Add to …" button opens three steps');
    check(snap.closed, 'pressing the open button again closes the steps');
    check(snap.copy[1] !== snap.copy[0] && snap.copy[2] === snap.copy[0], `Copy → ${snap.copy[1]} → back (${snap.copy.join(' → ')})`);
    check(snap.clipboard === 'https://connector.tgmcp.dev/mcp', 'the link is copied');
    check(snap.stars.shown && (snap.lang === 'en' ? snap.stars.count === '1.2k' : /^1[.,]2/.test(snap.stars.count)) && /1234/.test(snap.stars.label), `star count shows (${snap.stars.count}, "${snap.stars.label}")`);
    if (snap.lang !== 'en') {
      const english = snap.apps.flatMap((a) => a.steps).filter((t) => !/[а-яё]/i.test(t));
      check(english.length === 0, `${snap.lang}: every install step comes from the page's dictionary ${english.length ? JSON.stringify(english) : ''}`);
    }
    const caps = new Set(snap.frames.map((f) => f.split('\n').pop()));
    check(caps.size === 3, `the demo plays three scenarios (${[...caps].join(', ')})`);
    if (snapOut) fs.writeFileSync(snapOut, JSON.stringify(snap, null, 1));
    await fallback(browser, url, origin);
    await noScript(browser, url);
    await seo(browser, url);
  } finally {
    await browser.close();
    server.close();
  }
  console.log(failures.length ? `\n${failures.length} check(s) failed` : '\nall checks passed');
  process.exit(failures.length ? 1 : 0);
})();
