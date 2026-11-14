// Renders the server's icon PNGs from its SVG master (#61). Run from the repo
// root with Node and Playwright's Chromium:
//
//   node assets/icon/render.cjs
//
// It copies site/favicon.svg (the tg-mcp mark, the master) to icon.svg, then
// draws it at 32, 128, 256 and 512 px on a transparent background. Commit
// what changed; the server embeds these files (src/icon.rs).
const fs = require("fs");
const path = require("path");
const { chromium } = require("playwright");

const here = __dirname;
const master = path.join(here, "..", "..", "site", "favicon.svg");
const sizes = { "favicon-32.png": 32, "icon-128.png": 128, "icon-256.png": 256, "icon-512.png": 512 };

(async () => {
  const svg = fs.readFileSync(master, "utf8");
  fs.writeFileSync(path.join(here, "icon.svg"), svg);
  const browser = await chromium.launch();
  const page = await browser.newPage({ deviceScaleFactor: 1 });
  for (const [file, px] of Object.entries(sizes)) {
    const sized = svg.replace(/width="\d+" height="\d+"/, `width="${px}" height="${px}"`);
    await page.setViewportSize({ width: px, height: px });
    await page.setContent(
      `<!doctype html><html><body style="margin:0;background:transparent">${sized}</body></html>`
    );
    await page.screenshot({
      path: path.join(here, file),
      omitBackground: true,
      clip: { x: 0, y: 0, width: px, height: px },
    });
    console.log(`${file}: ${px}x${px}`);
  }
  await browser.close();
})();
