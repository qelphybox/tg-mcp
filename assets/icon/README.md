# The server's icon (#61)

The tg-mcp mark: a dark rounded square, a speech bubble in light ink and an orange dot. It is deliberately not Telegram's paper plane and doesn't use Telegram's blue (Telegram API ToS 2.4).

- **Master:** `site/favicon.svg`, the landing page's favicon. Change the mark there, nowhere else.
- **Here:** `icon.svg` is a copy of the master, and the PNGs are rendered from it: `favicon-32.png` (served as `/favicon.ico`), `icon-128.png` (`/icon.png`), `icon-256.png` and `icon-512.png`. They are transparent outside the rounded square. The server embeds these files (`src/icon.rs`) and draws nothing at runtime.
- **Regenerate** after changing the master: `node assets/icon/render.cjs` from the repo root. It needs Node and Playwright's Chromium; the cloud sessions have both, and elsewhere `npm i playwright && npx playwright install chromium` works. Commit the files that change. `cargo test` fails while `icon.svg` differs from the master.
