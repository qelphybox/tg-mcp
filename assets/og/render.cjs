// Renders the landing page's Open Graph image (1200x630 JPEG) for a language. Run from
// the repo root with Node and Playwright's Chromium:
//
//   node assets/og/render.cjs ru            # → site/ru/og.jpg
//   node assets/og/render.cjs en out.jpg    # the English text, to compare with site/og.jpg
//
// The design is the English site/og.jpg's: the wallpaper pattern on black, the mark,
// an eyebrow, the headline in Unbounded, the three apps and the domain. Fonts and the
// pattern come from site/, so nothing is fetched.
const fs = require("fs");
const path = require("path");
const http = require("http");
const { chromium } = require("playwright");

const site = path.join(__dirname, "..", "..", "site");
const TEXT = {
  en: { lang: "en", eyebrow: "Telegram MCP server · open source", title: "Tell your AI what to do in Telegram.", out: "og.jpg" },
  ru: { lang: "ru", eyebrow: "MCP-сервер для Telegram · открытый код", title: "Скажите ИИ, что сделать в Telegram.", out: "ru/og.jpg" },
};

const lang = process.argv[2] || "ru";
const t = TEXT[lang];
if (!t) throw new Error(`unknown language ${lang}: ${Object.keys(TEXT).join(", ")}`);
const out = path.resolve(process.argv[3] || path.join(site, t.out));

const page = `<!doctype html><html lang="${t.lang}"><head><meta charset="utf-8"><style>
@font-face{font-family:'Golos Text';font-weight:400 600;src:url(/fonts/golos-text-latin.woff2) format('woff2');unicode-range:U+0000-00FF,U+2000-206F}
@font-face{font-family:'Golos Text';font-weight:400 600;src:url(/fonts/golos-text-cyrillic.woff2) format('woff2');unicode-range:U+0400-045F}
@font-face{font-family:'Unbounded';font-weight:500 700;src:url(/fonts/unbounded-latin.woff2) format('woff2');unicode-range:U+0000-00FF,U+2000-206F}
@font-face{font-family:'Unbounded';font-weight:500 700;src:url(/fonts/unbounded-cyrillic.woff2) format('woff2');unicode-range:U+0400-045F}
@font-face{font-family:'JetBrains Mono';font-weight:400 500;src:url(/fonts/jetbrains-mono-latin.woff2) format('woff2')}
html,body{margin:0;width:1200px;height:630px;background:#000;overflow:hidden}
.fade{position:absolute;inset:0;background:linear-gradient(90deg,#000 0%,rgba(0,0,0,.75) 35%,rgba(0,0,0,0) 85%)}
.wall{position:absolute;inset:0;opacity:.2;background:#B4A9C4;-webkit-mask-image:url(/pattern.svg);-webkit-mask-size:420px auto}
.card{position:absolute;inset:0;padding:72px 80px 72px;box-sizing:border-box;display:flex;flex-direction:column;color:#F1ECF7;font-family:'Golos Text',sans-serif}
.brand{display:flex;align-items:center;gap:14px;font-family:Unbounded;font-weight:600;font-size:28px}
.eyebrow{margin:auto 0 18px;font-size:26px;font-weight:500;color:#E4B2EA}
h1{margin:0;font-family:Unbounded;font-weight:600;font-size:${lang === "ru" ? 68 : 76}px;line-height:1.08;letter-spacing:-0.03em;max-width:${lang === "ru" ? 1000 : 900}px}
.foot{margin-top:auto;display:flex;align-items:center;gap:14px}
.chip{padding:10px 20px;border-radius:12px;border:1px solid rgba(241,236,247,.18);background:rgba(30,25,40,.7);font-size:24px}
.dom{margin-left:auto;font-family:'JetBrains Mono';font-size:25px;color:#C9B3FF}
</style></head><body><div class="wall"></div><div class="fade"></div><div class="card">
<div class="brand"><svg width="34" height="34" viewBox="0 0 26 26" fill="none"><rect x="1.5" y="3.5" width="23" height="17" rx="6" stroke="#F1ECF7" stroke-width="2"/><path d="M7 20.5 6 24.5l5-4" stroke="#F1ECF7" stroke-width="2" stroke-linejoin="round"/><circle cx="19" cy="7" r="4" fill="#E4B2EA" stroke="#000" stroke-width="2"/></svg>tg-mcp</div>
<div class="eyebrow">${t.eyebrow}</div>
<h1>${t.title}</h1>
<div class="foot"><span class="chip">Claude</span><span class="chip">ChatGPT</span><span class="chip">Gemini</span><span class="dom">tgmcp.dev</span></div>
</div></body></html>`;

(async () => {
  const server = http.createServer((req, res) => {
    if (req.url === "/") { res.writeHead(200, { "content-type": "text/html; charset=utf-8" }); return res.end(page); }
    const file = path.join(site, decodeURIComponent(req.url));
    if (!file.startsWith(site) || !fs.existsSync(file)) { res.writeHead(404); return res.end(); }
    res.writeHead(200, { "content-type": file.endsWith(".svg") ? "image/svg+xml" : "font/woff2" });
    fs.createReadStream(file).pipe(res);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const browser = await chromium.launch();
  const tab = await browser.newPage({ viewport: { width: 1200, height: 630 }, deviceScaleFactor: 1 });
  await tab.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: "networkidle" });
  await tab.evaluate(() => document.fonts.ready);
  fs.mkdirSync(path.dirname(out), { recursive: true });
  await tab.screenshot({ path: out, type: "jpeg", quality: 88 });
  await browser.close();
  server.close();
  console.log(`${path.relative(process.cwd(), out)}: 1200x630`);
})();
