# Deploy

Production runs at `https://connector.tgmcp.dev` on one VPS, deployed with [Kamal 2](https://kamal-deploy.org): `config/deploy.yml`, `.kamal/secrets`, `.github/workflows/deploy.yml`.

## How a rollout works

1. GitHub Actions builds the image from `Dockerfile` and pushes it to `ghcr.io/qelphybox/tg-mcp:<git sha>`.
2. Kamal SSHes into the server, pulls the image and starts the new container next to the old one.
3. kamal-proxy polls `/healthz` on the new container; once it answers, traffic switches over. The old container drains for up to `drain_timeout` (30 s), then gets SIGTERM and has `stop_timeout` (30 s) to exit before SIGKILL. On SIGTERM it closes MCP streams, waits for in-flight requests, then closes every Live client and re-seals each Telegram session that changed while live (bounded to 10 s; logged as `client pool flushed` with counts).

Triggers: every push to `main`, or **Actions → Deploy → Run workflow** (pick `setup` for a fresh server, `deploy` otherwise). Manual runs only appear once the workflow is on `main`.

kamal-proxy terminates TLS with a Let's Encrypt certificate for the host. `text/event-stream` responses (MCP streams) pass through unbuffered; `response_timeout` only bounds the wait for response headers.

Client IPs: the app only sees kamal-proxy as its TCP peer. kamal-proxy (with `ssl: true` and `forward_headers` unset, so false) drops any `X-Forwarded-For` the client sent and sets it to the address it accepted the connection from; with `forward_headers: true` it would append that address instead. Either way the right-most entry is the real client, and `TG_MCP_TRUST_FORWARDED_FOR=1` makes the app read it for the per-IP limits on `/register`, `/authorize`, `/token`, `/revoke`, `/media/{ticket}` and on pending logins. Don't set kamal-proxy's `client_ip_header`: it would copy a client-controlled header into `X-Forwarded-For`.

Media links (`get_media`, #36): `GET /media/{ticket}` carries its credential in the path, because the agent fetching it with curl has no token. The app never logs tickets, URLs or file bytes, but kamal-proxy's access log (`kamal proxy logs`) records request paths, so a ticket can end up there. This was accepted: a ticket works for 5 minutes and 3 downloads at most, for one file, and the proxy log stays on the server, where whoever can read it can read the app's memory too. Response buffering is off in `config/deploy.yml` (`proxy.buffering.responses: false`): by default kamal-proxy holds a response whole and writes anything over 1 MB to disk, which would put downloaded files on the server's disk. Downloads stream through it with their `Content-Length`.

## What survives a deploy

- **Survives:** everything in SQLite (Grants, Tokens, encrypted Telegram sessions) on the `tg-mcp-data` volume, mounted at `/data`.
- **Lost:** in-memory state: open MCP sessions (clients re-initialize on their own), Live clients (reconnect lazily on the next call; Telegram session changes they held, such as a DC migration, are re-sealed on a graceful shutdown, but lost on SIGKILL or a crash), authorization codes and logins in progress (the user starts the browser flow again), Media links (their downloads 404; the agent calls `get_media` again), the duplicate guard of `send_message` (a text sent in the minute before the deploy can be sent again right after it), rate-limit counters (they start full again).

## First deploy

Run the wizard from a local clone (needs `gh` logged in) and follow it:

```sh
scripts/setup-deploy.sh
```

It collects the Telegram api_id/api_hash, makes sure your personal key opens `root@<server>`, waits for the DNS A record, generates a dedicated deploy key, hardens the server with `scripts/harden-server.sh` (see below), pins the server's SSH host key and triggers the first deploy. It writes these to GitHub (and the same values to your local, git-ignored `.env`):

| Name | Kind | What |
| --- | --- | --- |
| `TG_MCP_API_ID`, `TG_MCP_API_HASH` | secret | Telegram app credentials (my.telegram.org) |
| `SSH_PRIVATE_KEY` | secret | Deploy key, `~/.ssh/tg-mcp-deploy` |
| `KAMAL_HOST`, `KAMAL_SSH_USER` | variable | Server IP and `deploy` |
| `SSH_KNOWN_HOSTS` | variable | Pinned host key; CI refuses any other |

The registry password is the workflow's own `GITHUB_TOKEN`; nothing to set. Re-running the wizard is safe: it keeps saved values and the hardening script is idempotent.

## Server baseline

`scripts/harden-server.sh` (Debian/Ubuntu, run as root, idempotent) leaves the server like this:

- **SSH:** public keys only (passwords and keyboard-interactive off), root allowed with a key only, `AllowUsers root deploy`, 3 attempts per connection. The config is a drop-in named `00-tg-mcp.conf` so it beats cloud-init's `50-cloud-init.conf` (sshd keeps the first value it reads), and it is checked with `sshd -t` before the reload. The script refuses to run if root has no key, so it can't lock you out.
- **`deploy` user:** what Kamal and CI log in as. Member of `docker`, password locked, only the CI deploy key. Note that `docker` group membership is root-equivalent: the point is a separate, revocable key, not a sandbox. You keep administering as root with your personal key.
- **Firewall (ufw):** incoming denied except 22 (rate-limited), 80 and 443.
- **fail2ban:** sshd jail, 5 failures in 10 min → 1 h ban.
- **Updates:** unattended-upgrades for the distro security pocket and Docker CE; reboots at 04:30 UTC when an update needs it. Containers come back on their own (`--restart unless-stopped`); in-memory state is lost as on a deploy.
- **Docker CE** from download.docker.com, daemon with `live-restore` (containers survive a dockerd upgrade), `no-new-privileges`, and log rotation.

Caveat: Docker writes its own iptables rules, so a port published with `-p` bypasses ufw. Kamal publishes only kamal-proxy's 80/443 and keeps the app on the internal Docker network, so nothing else is exposed. Don't add `publish`/`-p` options without a firewall rule in mind.

## Day-to-day

From a local clone with the variables above exported (`set -a; . ./.env; set +a`) and `KAMAL_REGISTRY_PASSWORD` set to a GitHub token with `read:packages`:

```sh
kamal app logs -f        # follow logs
kamal rollback <sha>     # switch back to a previous image (kamal app containers lists them)
```

A rollback can't cross a database migration: an image older than a migration the database already ran refuses to start (sqlx: "migration N was previously applied but is missing"). Check `migrations/` between the two commits; if one was added, fix forward instead.

The image is distroless (no shell), so `kamal app exec` with a shell does not work, by design.

## Landing page

`https://tgmcp.dev` is GitHub Pages, published from `site/` by `.github/workflows/pages.yml` on every push to `main` that touches it. There is no build step: what is in `site/` is what is served. The custom domain lives in **Settings → Pages**, not in a `CNAME` file (Pages ignores it when deploying from Actions).

DNS for `tgmcp.dev`, at its registrar:

| Name | Type | Value |
| --- | --- | --- |
| `@` | A | `185.199.108.153`, `185.199.109.153`, `185.199.110.153`, `185.199.111.153` |
| `@` | AAAA | `2606:50c0:8000::153`, `2606:50c0:8001::153`, `2606:50c0:8002::153`, `2606:50c0:8003::153` |
| `www` | CNAME | `qelphybox.github.io` |
| `connector` | A | the server's IPv4 (`KAMAL_HOST`), no CDN proxying |

`.dev` is on the HSTS preload list, so browsers only ever use HTTPS: the site and the connector open once their certificates are issued (Pages: after the domain check in Settings → Pages; kamal-proxy: on the first HTTPS request).

## Releases

Production follows `main`; releases are for self-hosters. Bump `version` in `Cargo.toml`, merge, then tag:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` checks the tag matches `Cargo.toml`, builds `linux/amd64` and `linux/arm64` (the Dockerfile cross-compiles, no emulation), pushes `ghcr.io/qelphybox/tg-mcp:{0.1.0,0.1,latest}` and creates a GitHub Release with generated notes. Run [the acceptance checks](acceptance.md) before tagging and put the memory numbers in the release notes.

The GHCR package is private until its visibility is set to public once (GitHub → Packages → tg-mcp → Package settings).
